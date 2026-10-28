//! Triage routing, ported from Python's `orchestration/router.py`.
//!
//! With `[model] router = "triage"`, each turn goes to `[model.route] cheap` or
//! `[model.route] frontier` by a fast deterministic heuristic (no extra model call),
//! so simple asks don't pay frontier prices. Providers are built on first use and
//! cached by model ref. `/model` pins one provider, which turns routing off for the
//! rest of the session, as Python's `set_forced` does. With routing off every turn
//! uses the session's model.
//!
//! The model a turn was routed to stays the active one after the turn, as Python's
//! `self.provider` does: the status line, `/cost`'s pricing, and the next request's
//! window and output cap follow it. Compaction summaries are written by the cheap
//! route when routing is on.

use crate::agent::Harness;
use crate::memory::working::python_strip;
use crate::provider::{Provider, ProviderError};
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// Signals that a task is non-trivial and deserves the frontier model.
static COMPLEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(code|coding|program|script|debug|refactor|analy[sz]e|implement|plan|design|architect|multi[- ]?step|step by step|research|reason|prove|optimi[sz]e|build|fix|migrate|review|explain why)\b",
    )
    .expect("valid")
});

/// Tasks longer than this go to the frontier model.
pub const SIMPLE_MAX_CHARS: usize = 240;

/// Python's `is_simple`: short and free of complexity keywords goes to the cheap model.
pub fn is_simple(task: &str) -> bool {
    !COMPLEX.is_match(task) && python_strip(task).chars().count() <= SIMPLE_MAX_CHARS
}

/// A model a turn can be routed to, with what the harness derives from its ref.
#[derive(Clone)]
pub struct RoutedModel {
    /// The `provider:model` ref.
    pub reference: String,
    pub provider: Arc<dyn Provider>,
    /// `max_output_tokens_for` the ref's provider section.
    pub max_tokens: u32,
    /// `context_window_for` the ref.
    pub context_window: u64,
    /// The `[table] key` its output cap comes from.
    pub output_setting: String,
}

/// Builds a [`RoutedModel`] for a ref, or says why it cannot (Python's
/// `provider_for_model` raising).
pub type ModelBuilder = Box<dyn Fn(&str) -> Result<RoutedModel, String> + Send + Sync>;

/// The routing policy and its provider cache. [`Router::default`] never routes.
#[derive(Default)]
pub struct Router {
    /// `[model] router = "triage"`.
    pub triage: bool,
    pub cheap: String,
    pub frontier: String,
    /// `/model` pinned a provider: routing is off for the rest of the session.
    pinned: bool,
    build: Option<ModelBuilder>,
    cache: Mutex<HashMap<String, RoutedModel>>,
    /// The model the last routed turn used.
    current: Mutex<Option<RoutedModel>>,
}

impl Router {
    pub fn new(triage: bool, cheap: &str, frontier: &str, build: ModelBuilder) -> Self {
        Self {
            triage,
            cheap: cheap.to_owned(),
            frontier: frontier.to_owned(),
            build: Some(build),
            ..Self::default()
        }
    }

    /// Python's `enabled`: triage is configured and no provider is pinned.
    pub fn enabled(&self) -> bool {
        self.triage && !self.pinned && self.build.is_some()
    }

    /// The route for `task`, as Python's `select_model` with routing on.
    pub fn select(&self, task: &str) -> &str {
        if is_simple(task) {
            &self.cheap
        } else {
            &self.frontier
        }
    }

    /// The model for `reference`, built once and cached.
    pub fn model_for(&self, reference: &str) -> Result<RoutedModel, String> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(model) = cache.get(reference) {
            return Ok(model.clone());
        }
        let build = self.build.as_ref().ok_or("routing is not configured")?;
        let model = build(reference)?;
        cache.insert(reference.to_owned(), model.clone());
        Ok(model)
    }

    /// The model the last routed turn used, while routing is on.
    pub fn current(&self) -> Option<RoutedModel> {
        if !self.enabled() {
            return None;
        }
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_current(&self, model: RoutedModel) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(model);
    }

    /// Pin the session's provider (`/model`), as Python's `invalidate` then
    /// `set_forced`: routing stops and the cached providers are dropped, so a later
    /// build sees the current keys and addresses.
    pub fn pin(&mut self) {
        self.pinned = true;
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl Harness {
    /// The provider the next request goes to: the routed one, else the session's.
    pub fn active_provider(&self) -> Arc<dyn Provider> {
        self.router
            .current()
            .map_or_else(|| Arc::clone(&self.provider), |model| model.provider)
    }

    /// The ref the last turn was routed to, while routing is on.
    pub fn routed_ref(&self) -> Option<String> {
        self.router.current().map(|model| model.reference)
    }

    /// The configured output cap of the active model.
    pub(crate) fn configured_output(&self) -> u32 {
        self.router
            .current()
            .map_or(self.max_tokens, |model| model.max_tokens)
    }

    /// The context window of the active model.
    pub fn active_context_window(&self) -> u64 {
        self.router
            .current()
            .map_or(self.context_window, |model| model.context_window)
    }

    /// The `[table] key` the active model's output cap comes from.
    pub(crate) fn configured_output_setting(&self) -> Option<String> {
        self.router.current().map(|model| model.output_setting)
    }

    /// Pick this turn's model, as the routing step of Python's `run_turn`: with
    /// routing on, the cheap or frontier route by the task, recorded as a `route`
    /// event. A route that cannot be built fails the turn, as in Python.
    pub(crate) fn route(&self, task: &str, sid: &str) -> Result<(), ProviderError> {
        if !self.router.enabled() {
            return Ok(());
        }
        let reference = self.router.select(task).to_owned();
        let model = self
            .router
            .model_for(&reference)
            .map_err(ProviderError::Message)?;
        let name = model.provider.model().to_owned();
        self.router.set_current(model);
        self.recorder
            .event(sid, "route", vec![("model", json!(name))]);
        Ok(())
    }

    /// The model that writes compaction summaries, as Python's `_summary_provider`:
    /// the cheap route when routing is on (and it can be built), else the active one.
    pub(crate) fn summary_provider(&self) -> Arc<dyn Provider> {
        if self.router.enabled() {
            if let Ok(model) = self.router.model_for(&self.router.cheap) {
                return model.provider;
            }
        }
        self.active_provider()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_plain_asks_are_simple() {
        assert!(is_simple("What's the capital of France?"));
        assert!(is_simple(&"a".repeat(SIMPLE_MAX_CHARS)));
        // Surrounding whitespace does not count, as Python's `strip()`.
        assert!(is_simple(&format!(
            "  {}  \n",
            "a".repeat(SIMPLE_MAX_CHARS)
        )));
        assert!(!is_simple(&"a".repeat(SIMPLE_MAX_CHARS + 1)));
        // Code points, not bytes.
        assert!(is_simple(&"é".repeat(SIMPLE_MAX_CHARS)));
    }

    #[test]
    fn complexity_keywords_go_to_the_frontier_model() {
        for task in [
            "fix the typo",
            "Please REVIEW this",
            "analyse the logs",
            "analyze the logs",
            "a multi-step job",
            "a multistep job",
            "a multi step job",
            "think step by step",
            "explain why it fails",
            "optimise it",
            "write a script",
        ] {
            assert!(!is_simple(task), "{task}");
        }
        // Whole words only: `\b` on both sides.
        for task in ["prefix the list", "codes", "planet names", "builder hours"] {
            assert!(is_simple(task), "{task}");
        }
    }
}
