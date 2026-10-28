//! Turn limits changed while agent86 runs: a deliberate addition, since Python
//! reads `[limits] max_steps`, `[limits] max_output_tokens`, the provider's
//! `max_tokens`, and `[agents] max_steps` only from config.
//!
//! Three limits can be overridden for the rest of the process, by `/limits`, by
//! `run`'s `--max-steps`, `--max-output-tokens`, and `--max-agent-steps`, or by
//! answering the interface's offer when a turn reaches one:
//!
//! - `steps`: the turn's step budget (`[limits] max_steps`)
//! - `agent_steps`: each sub-agent's step budget (`[agents] max_steps`), never
//!   above `steps`, as Python's `CircuitBreaker` clamps it
//! - `output`: the output cap of every model call, the main loop's and the
//!   sub-agents'. It replaces whichever setting Python's `max_output_tokens_for`
//!   would pick (the provider's `max_tokens`, else `[limits] max_output_tokens`),
//!   so a raised cap always takes effect.
//!
//! An override outlives `/model`, `/clear`, and `/resume`. The offer is a hook,
//! like the approval prompt: the interface installs it, and without it (`run`, the
//! plain loop) a limit reached ends the turn or the sub-agent as before.

use crate::agent::Harness;
use crate::config::Config;
use crate::config_writer::Change;
use serde_json::json;
use std::sync::{Mutex, PoisonError};

/// One limit `/limits` can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Steps,
    AgentSteps,
    Output,
}

impl LimitKind {
    pub const ALL: [Self; 3] = [Self::Steps, Self::AgentSteps, Self::Output];

    /// The name `/limits` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Steps => "steps",
            Self::AgentSteps => "agent_steps",
            Self::Output => "output",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// What the limit counts.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Steps | Self::AgentSteps => "steps",
            Self::Output => "tokens",
        }
    }
}

/// The overridden limits; `None` leaves a limit to the config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitOverrides {
    pub steps: Option<u32>,
    pub agent_steps: Option<u32>,
    pub output: Option<u32>,
}

impl LimitOverrides {
    pub fn get(&self, kind: LimitKind) -> Option<u32> {
        match kind {
            LimitKind::Steps => self.steps,
            LimitKind::AgentSteps => self.agent_steps,
            LimitKind::Output => self.output,
        }
    }

    fn slot(&mut self, kind: LimitKind) -> &mut Option<u32> {
        match kind {
            LimitKind::Steps => &mut self.steps,
            LimitKind::AgentSteps => &mut self.agent_steps,
            LimitKind::Output => &mut self.output,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A limit a running turn reached, put to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitHit {
    pub kind: LimitKind,
    /// The sub-agent's role, or `None` for the main loop.
    pub agent: Option<String>,
    /// The limit that was reached.
    pub current: u32,
    /// The values offered, ascending.
    pub offers: Vec<u32>,
}

impl LimitHit {
    /// What happened, in one line.
    pub fn headline(&self) -> String {
        let current = self.current;
        match (self.kind, &self.agent) {
            (LimitKind::Steps | LimitKind::AgentSteps, None) => {
                format!("Step budget reached ({current} steps)")
            }
            (LimitKind::Steps | LimitKind::AgentSteps, Some(role)) => {
                format!("Sub-agent '{role}' reached its step budget ({current} steps)")
            }
            (LimitKind::Output, None) => {
                format!("The reply reached the output limit ({current} tokens) before any text")
            }
            (LimitKind::Output, Some(role)) => format!(
                "Sub-agent '{role}' reached the output limit ({current} tokens) before any text"
            ),
        }
    }

    /// What accepting does.
    pub fn question(&self) -> &'static str {
        match self.kind {
            LimitKind::Steps | LimitKind::AgentSteps => "Raise it for this session and go on?",
            LimitKind::Output => "Raise it for this session and ask again?",
        }
    }
}

/// Asks whether to raise a limit that was reached: the new value, or `None` to stop.
pub type LimitPrompt = Box<dyn Fn(&LimitHit) -> Option<u32> + Send + Sync>;

/// The overrides, behind a lock so a turn holding the harness can raise one, and
/// the hook that offers to.
#[derive(Default)]
pub struct SessionLimits {
    overrides: Mutex<LimitOverrides>,
    pub prompt: Option<LimitPrompt>,
    /// The `[table] key` the configured output cap comes from, for messages.
    configured_output: String,
}

impl SessionLimits {
    pub fn overrides(&self) -> LimitOverrides {
        *self
            .overrides
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, kind: LimitKind) -> Option<u32> {
        self.overrides().get(kind)
    }

    pub fn set(&self, kind: LimitKind, value: Option<u32>) {
        let mut overrides = self
            .overrides
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *overrides.slot(kind) = value;
    }

    pub fn clear(&self) {
        *self
            .overrides
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = LimitOverrides::default();
    }

    /// What to raise for a larger output cap: the session's override when there is
    /// one, else the setting the configured cap comes from.
    pub fn output_setting(&self) -> String {
        if self.get(LimitKind::Output).is_some() {
            "the session's output limit (/limits output)".to_owned()
        } else if self.configured_output.is_empty() {
            "[limits] max_output_tokens".to_owned()
        } else {
            self.configured_output.clone()
        }
    }
}

/// What to offer for a limit at `current`: double and quadruple it, kept at or
/// under `ceiling` and above `current`.
pub fn offers(current: u32, ceiling: Option<u32>) -> Vec<u32> {
    let mut values: Vec<u32> = [current.saturating_mul(2), current.saturating_mul(4)]
        .into_iter()
        .map(|value| ceiling.map_or(value, |ceiling| value.min(ceiling)))
        .filter(|&value| value > current)
        .collect();
    values.dedup();
    values
}

/// The setting the output cap comes from in `config`, as Python's
/// `max_output_tokens_for` chooses it, for the provider section `provider`.
pub fn output_setting(config: &Config, provider: &str) -> (Vec<String>, String) {
    let per_provider = config
        .providers
        .get(provider)
        .and_then(|section| section.max_tokens)
        .is_some_and(|cap| cap > 0);
    if per_provider {
        (
            vec!["providers".into(), provider.into(), "max_tokens".into()],
            format!("[providers.{provider}] max_tokens"),
        )
    } else {
        (
            vec!["limits".into(), "max_output_tokens".into()],
            "[limits] max_output_tokens".into(),
        )
    }
}

/// The config key path and its `[table] key` label for `kind`.
pub fn setting(kind: LimitKind, config: &Config, provider: &str) -> (Vec<String>, String) {
    match kind {
        LimitKind::Steps => (
            vec!["limits".into(), "max_steps".into()],
            "[limits] max_steps".into(),
        ),
        LimitKind::AgentSteps => (
            vec!["agents".into(), "max_steps".into()],
            "[agents] max_steps".into(),
        ),
        LimitKind::Output => output_setting(config, provider),
    }
}

/// What `config` sets `kind` to for the provider section `provider`.
pub fn configured(kind: LimitKind, config: &Config, provider: &str) -> u32 {
    match kind {
        LimitKind::Steps => config.limits.max_steps,
        LimitKind::AgentSteps => config.agents.max_steps,
        LimitKind::Output => config.max_output_tokens_for(provider),
    }
}

/// The config changes that keep `overrides`: each one written to the setting it
/// replaces, so the saved value is the one that takes effect. The output cap goes
/// to the provider's `max_tokens` when that section sets one, else to
/// `[limits] max_output_tokens`.
pub fn save_changes(overrides: LimitOverrides, config: &Config, provider: &str) -> Vec<Change> {
    LimitKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let value = overrides.get(kind)?;
            let value = toml_edit::Value::from(i64::from(value));
            Some((setting(kind, config, provider).0, Some(value)))
        })
        .collect()
}

impl Harness {
    /// The turn's step budget.
    pub fn max_steps(&self) -> u32 {
        self.session_limits
            .get(LimitKind::Steps)
            .unwrap_or(self.limits.max_steps)
    }

    /// A sub-agent's step budget, never above the turn's.
    pub fn agent_max_steps(&self) -> u32 {
        self.session_limits
            .get(LimitKind::AgentSteps)
            .unwrap_or(self.agents.max_steps)
            .min(self.max_steps())
    }

    /// The output cap sent with every model call.
    pub fn output_cap(&self) -> u32 {
        self.session_limits
            .get(LimitKind::Output)
            .unwrap_or(self.max_tokens)
    }

    /// The effective value of `kind`.
    pub fn limit(&self, kind: LimitKind) -> u32 {
        match kind {
            LimitKind::Steps => self.max_steps(),
            LimitKind::AgentSteps => self.agent_max_steps(),
            LimitKind::Output => self.output_cap(),
        }
    }

    /// Take the configured output cap for the provider section `provider`, as
    /// Python's `max_output_tokens_for`: at start and on every model switch.
    pub fn configure_output(&mut self, config: &Config, provider: &str) {
        self.max_tokens = config.max_output_tokens_for(provider);
        self.session_limits.configured_output = output_setting(config, provider).1;
    }

    /// Take the limits from a freshly loaded `config` for the provider section
    /// `provider`, dropping the overrides it now agrees with (after a save).
    pub fn reload_limits(&mut self, config: &Config, provider: &str) {
        self.limits = config.limits.clone();
        self.agents = config.agents.clone();
        self.configure_output(config, provider);
        for kind in LimitKind::ALL {
            if self.session_limits.get(kind) == Some(configured(kind, config, provider)) {
                self.session_limits.set(kind, None);
            }
        }
    }

    /// Offer to raise `kind`, reached at `current`, and keep the new value for the
    /// session. `None` when there is no hook, nothing to offer, or the user stops.
    pub(crate) fn raise_limit(
        &self,
        kind: LimitKind,
        agent: Option<&str>,
        sid: &str,
        current: u32,
    ) -> Option<u32> {
        let prompt = self.session_limits.prompt.as_ref()?;
        // A sub-agent's budget stays under the turn's.
        let ceiling = (kind == LimitKind::AgentSteps).then(|| self.max_steps());
        let hit = LimitHit {
            kind,
            agent: agent.map(str::to_owned),
            current,
            offers: offers(current, ceiling),
        };
        if hit.offers.is_empty() {
            return None;
        }
        let raised = prompt(&hit).filter(|value| hit.offers.contains(value))?;
        self.session_limits.set(kind, Some(raised));
        let mut fields = vec![("limit", json!(kind.name()))];
        if let Some(agent) = agent {
            fields.push(("agent", json!(agent)));
        }
        fields.push(("from", json!(current)));
        fields.push(("to", json!(raised)));
        self.recorder.event(sid, "limit_raised", fields);
        Some(raised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, ConfigPaths, Overrides};

    fn config(name: &str, text: &str) -> Config {
        let root =
            std::env::temp_dir().join(format!("agent86-limits-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.toml"), text).unwrap();
        let paths = ConfigPaths {
            user: None,
            project: root.join("config.toml"),
        };
        let config = load_config(&paths, &Overrides::default()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        config
    }

    #[test]
    fn offers_double_and_quadruple_under_the_ceiling() {
        assert_eq!(offers(40, None), vec![80, 160]);
        assert_eq!(offers(8192, None), vec![16384, 32768]);
        assert_eq!(offers(8, Some(20)), vec![16, 20]);
        assert_eq!(offers(8, Some(10)), vec![10]);
        assert_eq!(offers(40, Some(40)), Vec::<u32>::new());
        assert_eq!(offers(u32::MAX, None), Vec::<u32>::new());
    }

    #[test]
    fn names_parse_back() {
        for kind in LimitKind::ALL {
            assert_eq!(LimitKind::parse(kind.name()), Some(kind));
        }
        assert_eq!(LimitKind::parse("max_steps"), None);
    }

    #[test]
    fn the_output_cap_is_saved_where_it_takes_effect() {
        let overrides = LimitOverrides {
            steps: Some(80),
            agent_steps: None,
            output: Some(32000),
        };
        let written = |changes: Vec<Change>| -> Vec<(String, Option<i64>)> {
            changes
                .into_iter()
                .map(|(path, value)| (path.join("."), value.and_then(|value| value.as_integer())))
                .collect()
        };
        let plain = config("plain", "[model]\ndefault = \"openrouter:a\"\n");
        assert_eq!(
            written(save_changes(overrides, &plain, "openrouter")),
            vec![
                ("limits.max_steps".to_owned(), Some(80)),
                ("limits.max_output_tokens".to_owned(), Some(32000)),
            ]
        );
        // The provider's own cap wins over `[limits]`, so that is the one to change.
        let per_provider = config(
            "provider",
            "[model]\ndefault = \"openrouter:a\"\n[providers.openrouter]\nmax_tokens = 4096\n",
        );
        assert_eq!(
            written(save_changes(overrides, &per_provider, "openrouter"))[1],
            ("providers.openrouter.max_tokens".to_owned(), Some(32000))
        );
        assert_eq!(
            setting(LimitKind::Output, &per_provider, "openrouter").1,
            "[providers.openrouter] max_tokens"
        );
    }

    #[test]
    fn the_notice_names_the_setting_to_raise() {
        let mut limits = SessionLimits::default();
        assert_eq!(limits.output_setting(), "[limits] max_output_tokens");
        limits.configured_output = "[providers.anthropic] max_tokens".into();
        assert_eq!(limits.output_setting(), "[providers.anthropic] max_tokens");
        limits.set(LimitKind::Output, Some(32000));
        assert_eq!(
            limits.output_setting(),
            "the session's output limit (/limits output)"
        );
    }

    #[test]
    fn hits_say_what_was_reached() {
        let hit = |kind, agent: Option<&str>, current| LimitHit {
            kind,
            agent: agent.map(str::to_owned),
            current,
            offers: Vec::new(),
        };
        assert_eq!(
            hit(LimitKind::Steps, None, 40).headline(),
            "Step budget reached (40 steps)"
        );
        assert_eq!(
            hit(LimitKind::AgentSteps, Some("reviewer"), 8).headline(),
            "Sub-agent 'reviewer' reached its step budget (8 steps)"
        );
        assert_eq!(
            hit(LimitKind::Output, Some("reviewer"), 8192).headline(),
            "Sub-agent 'reviewer' reached the output limit (8192 tokens) before any text"
        );
    }
}
