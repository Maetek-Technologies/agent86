//! The interactive REPL, ported from Python's `ui/repl.py` (the plain `input()`
//! loop) and the slash-command registry both of Python's surfaces share,
//! `tui/commands.py`.
//!
//! Python's default surface is a full-screen Textual app, with this loop as the
//! fallback for `--plain`, `AGENT86_PLAIN`, piped stdin, or a terminal that cannot
//! host it. Here the default is the inline interface in `crate::ui`, which runs the
//! same registry on the same [`Repl`].
//!
//! Ctrl+C during a turn cancels it at the next safe point, as Python's full-screen
//! app does with its cancel key: the turn ends with `[cancelled]` and is saved. At
//! the prompt, Ctrl+C starts a fresh line; Ctrl+D (Ctrl+Z, Enter on Windows) quits.

use crate::agent::Harness;
use crate::approval::{stdout_prompt, ApprovalPrompt};
use crate::config::{Config, ConfigPaths};
use crate::history::PromptHistory;
use crate::mentions::expand_mentions;
use crate::pricing::{format_cost, PriceTable, UNPRICED_LABEL};
use crate::secrets::{SecretStore, SERVICE_NAME};
use crate::session::{AgentState, SessionInfo, SessionStore};
use crate::setup::{assemble, build_provider, SetupError, MODEL_HELP};
use crate::turn::format_turn_summary;
use crate::types::{ApprovalMode, ModelRef};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::mpsc;

/// What both surfaces show when the session log is not being kept.
pub const NO_MEMORY_NOTE: &str =
    "memory is disabled - sessions are not saved, so there is nothing to resume";

/// Prefixes of the harness's own mid-turn notices. They arrive as ordinary text, but
/// they are the harness talking about the conversation, not the model answering, so
/// they are set apart (dim, on their own line).
pub const NOTICE_PREFIXES: [&str; 5] = [
    "[compacted",
    "[compacting",
    "[compaction",
    "[continuation",
    "[continuing",
];

/// The notice carried by this text, stripped, or `None` if it is not one.
pub fn notice_text(text: &str) -> Option<&str> {
    let stripped = text.trim();
    NOTICE_PREFIXES
        .iter()
        .any(|prefix| stripped.starts_with(prefix))
        .then_some(stripped)
}

// --------------------------------------------------------------------------
// Styled text
// --------------------------------------------------------------------------

/// The few Rich styles the REPL uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Dim,
    Bold,
    Cyan,
    BoldCyan,
    Green,
    Yellow,
    Red,
    BoldRed,
    DimRed,
    DimYellow,
    Italic,
    BoldUnderline,
}

impl Style {
    pub(crate) fn ansi(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Dim => "\x1b[2m",
            Self::Bold => "\x1b[1m",
            Self::Cyan => "\x1b[36m",
            Self::BoldCyan => "\x1b[1;36m",
            Self::Green => "\x1b[32m",
            Self::Yellow => "\x1b[33m",
            Self::Red => "\x1b[31m",
            Self::BoldRed => "\x1b[1;31m",
            Self::DimRed => "\x1b[2;31m",
            Self::DimYellow => "\x1b[2;33m",
            Self::Italic => "\x1b[3m",
            Self::BoldUnderline => "\x1b[1;4m",
        }
    }
}

/// Text made of styled spans; what a slash command renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
    spans: Vec<(Style, String)>,
}

impl Text {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(Style::Plain, text)
    }

    pub fn styled(style: Style, text: impl Into<String>) -> Self {
        Self {
            spans: vec![(style, text.into())],
        }
    }

    pub fn push(&mut self, style: Style, text: impl Into<String>) -> &mut Self {
        self.spans.push((style, text.into()));
        self
    }

    pub fn append(&mut self, other: Text) -> &mut Self {
        self.spans.extend(other.spans);
        self
    }

    /// The styled pieces, in order.
    pub fn spans(&self) -> &[(Style, String)] {
        &self.spans
    }

    /// The text without styling.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|(_, text)| text.as_str()).collect()
    }

    /// The text with ANSI styling when `color` is on.
    pub fn render(&self, color: bool) -> String {
        self.spans
            .iter()
            .map(|(style, text)| {
                if color && *style != Style::Plain && !text.is_empty() {
                    format!("{}{text}\x1b[0m", style.ansi())
                } else {
                    text.clone()
                }
            })
            .collect()
    }
}

/// A plain aligned table; Python renders the same columns as a Rich table.
fn table(title: &str, headers: &[&str], rows: &[Vec<(Style, String)>]) -> Text {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (width, (_, cell)) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = Text::styled(Style::Bold, title);
    text.push(Style::Plain, "\n");
    let line = |cells: Vec<(Style, String)>, text: &mut Text| {
        let last = cells.len() - 1;
        for (index, (style, cell)) in cells.into_iter().enumerate() {
            let pad = if index == last {
                0
            } else {
                widths[index] - cell.chars().count() + 2
            };
            text.push(style, cell);
            text.push(Style::Plain, " ".repeat(pad));
        }
    };
    line(
        headers
            .iter()
            .map(|header| (Style::Bold, (*header).to_owned()))
            .collect(),
        &mut text,
    );
    for row in rows {
        text.push(Style::Plain, "\n");
        line(row.clone(), &mut text);
    }
    text
}

/// The startup identity panel, as Python's `banner(cfg, compact=False)`: a Rich
/// rounded panel titled `agent86`.
pub fn banner(config: &Config, version: &str) -> Text {
    panel(vec![
        vec![
            (Style::Plain, "version  ".into()),
            (Style::Cyan, version.into()),
        ],
        vec![
            (Style::Plain, "model    ".into()),
            (Style::Cyan, config.model.default.clone()),
            (Style::Plain, "   router ".into()),
            (Style::Cyan, config.model.router.as_str().into()),
        ],
        vec![
            (Style::Plain, "sandbox  ".into()),
            (Style::Cyan, config.sandbox.mode.as_str().into()),
            (Style::Plain, "   approval ".into()),
            (Style::Cyan, config.guardrails.approval.as_str().into()),
        ],
        vec![(Style::Dim, "Type /help for commands, /exit to quit.".into())],
    ])
}

/// The full-screen interface's banner, as Python's `banner(cfg)` (`compact=True`):
/// the version and the model; the status line reports the rest live.
pub fn compact_banner(config: &Config, version: &str) -> Text {
    panel(vec![
        vec![
            (Style::Plain, "version  ".into()),
            (Style::Cyan, version.into()),
        ],
        vec![
            (Style::Plain, "model    ".into()),
            (Style::Cyan, config.model.default.clone()),
        ],
    ])
}

/// A Rich rounded panel titled `agent86`, as wide as its widest row.
fn panel(rows: Vec<Vec<(Style, String)>>) -> Text {
    let width = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|(_, text)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    // Rich pads the title with a space each side and centres it in the top rule.
    let title = " agent86 ";
    let excess = width.saturating_sub(title.chars().count());
    let left = excess / 2;
    let mut text = Text::styled(
        Style::Cyan,
        format!(
            "╭─{}{title}{}─╮",
            "─".repeat(left),
            "─".repeat(excess - left)
        ),
    );
    for row in rows {
        let used: usize = row.iter().map(|(_, text)| text.chars().count()).sum();
        text.push(Style::Plain, "\n");
        text.push(Style::Cyan, "│ ");
        for (style, cell) in row {
            text.push(style, cell);
        }
        text.push(Style::Plain, " ".repeat(width - used));
        text.push(Style::Cyan, " │");
    }
    text.push(Style::Plain, "\n");
    text.push(Style::Cyan, format!("╰{}╯", "─".repeat(width + 2)));
    text
}

// --------------------------------------------------------------------------
// Slash commands
// --------------------------------------------------------------------------

/// What dispatching one input line did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A command ran; its output (if any) is in `render`.
    Handled,
    /// Not a command: run it as a turn.
    Turn,
    Exit,
    /// An empty line: nothing to run and nothing to say.
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub action: Action,
    pub render: Option<Text>,
    /// The conversation was reset, so the interface wipes its transcript and
    /// reprints the banner. Signalled here rather than by matching the command's
    /// name; the plain loop has no transcript and prints `render` regardless.
    pub clears_transcript: bool,
}

impl CommandResult {
    fn of(action: Action) -> Self {
        Self {
            action,
            render: None,
            clears_transcript: false,
        }
    }

    fn handled(render: Text) -> Self {
        Self {
            render: Some(render),
            ..Self::of(Action::Handled)
        }
    }

    fn message(text: impl Into<String>) -> Self {
        Self::handled(Text::plain(text))
    }
}

/// What a bare command (no argument) asks the interface to pick, as Python's
/// `ChoiceKind`. The plain loop ignores it and runs the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Model,
    Mode,
    ConfigModel,
    ConfigMcp,
    Resume,
}

/// One slash command, backing dispatch, `/help`, and the interface's command menu.
pub struct CommandEntry {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    handler: fn(&mut Repl, &str) -> CommandResult,
    pub needs_choice: Option<Choice>,
}

/// Python's registry, in its order.
pub const COMMANDS: [CommandEntry; 15] = [
    CommandEntry {
        name: "/help",
        usage: "/help",
        description: "Show this help",
        needs_choice: None,
        handler: |_, _| CommandResult::handled(help_table()),
    },
    CommandEntry {
        name: "/config",
        usage: "/config",
        description: "Show the resolved configuration",
        needs_choice: None,
        handler: |repl, _| CommandResult::message(repl.config_render()),
    },
    CommandEntry {
        name: "/config model",
        usage: "/config model",
        description: "Manage providers & models: add, key, test, switch, save",
        needs_choice: Some(Choice::ConfigModel),
        handler: |_, _| {
            let mut text = Text::plain("The model manager is a TUI surface — press / and pick ");
            text.push(Style::Cyan, "/config model");
            text.push(Style::Plain, ", or run agent86 without --plain.");
            CommandResult::handled(text)
        },
    },
    CommandEntry {
        name: "/config mcp",
        usage: "/config mcp",
        description: "Manage MCP servers: add, test, enable/disable, remove",
        needs_choice: Some(Choice::ConfigMcp),
        handler: |_, _| {
            let mut text = Text::plain("The MCP manager is a TUI surface — press / and pick ");
            text.push(Style::Cyan, "/config mcp");
            text.push(Style::Plain, ", or run agent86 without --plain.");
            CommandResult::handled(text)
        },
    },
    CommandEntry {
        name: "/models",
        usage: "/models",
        description: "List configured models",
        needs_choice: None,
        handler: |repl, _| CommandResult::handled(models_tables(&repl.config, &*repl.secrets)),
    },
    CommandEntry {
        name: "/model",
        usage: "/model <provider:model>",
        description: "Switch the active model for this session",
        needs_choice: Some(Choice::Model),
        handler: |repl, arg| CommandResult::message(repl.set_model(arg)),
    },
    CommandEntry {
        name: "/tools",
        usage: "/tools",
        description: "List available tools",
        needs_choice: None,
        handler: |repl, _| CommandResult::message(repl.tools_render()),
    },
    CommandEntry {
        name: "/skills",
        usage: "/skills",
        description: "List available skills",
        needs_choice: None,
        handler: |repl, _| {
            let skills = &repl.harness.context.skills;
            if skills.is_empty() {
                return CommandResult::message("no skills discovered");
            }
            let lines: Vec<String> = skills
                .values()
                .map(|skill| format!("{} - {}", skill.name, skill.description))
                .collect();
            CommandResult::message(lines.join("\n"))
        },
    },
    CommandEntry {
        name: "/memory",
        usage: "/memory",
        description: "Show memory stats and session id",
        needs_choice: None,
        handler: |repl, _| CommandResult::message(repl.memory_render()),
    },
    CommandEntry {
        name: "/mode",
        usage: "/mode [ask|auto|deny]",
        description: "Show/set approval mode (Shift+Tab cycles)",
        needs_choice: Some(Choice::Mode),
        handler: |repl, arg| CommandResult::message(repl.set_mode(arg)),
    },
    CommandEntry {
        name: "/cost",
        usage: "/cost",
        description: "Show token usage and cost this session",
        needs_choice: None,
        handler: |repl, _| CommandResult::message(repl.cost_render()),
    },
    CommandEntry {
        name: "/sessions",
        usage: "/sessions",
        description: "List recent sessions",
        needs_choice: None,
        handler: |repl, _| CommandResult::handled(repl.sessions_render(now())),
    },
    CommandEntry {
        name: "/resume",
        usage: "/resume [session-id]",
        description: "Resume a saved session",
        needs_choice: Some(Choice::Resume),
        handler: |repl, arg| repl.resume(arg),
    },
    CommandEntry {
        name: "/clear",
        usage: "/clear",
        description: "Start a fresh conversation",
        needs_choice: None,
        handler: |repl, _| {
            repl.state = repl.new_session();
            CommandResult {
                clears_transcript: true,
                ..CommandResult::message("conversation cleared")
            }
        },
    },
    CommandEntry {
        name: "/exit",
        usage: "/exit",
        description: "Quit",
        needs_choice: None,
        handler: |_, _| CommandResult::of(Action::Exit),
    },
];

fn help_table() -> Text {
    let width = COMMANDS
        .iter()
        .map(|entry| entry.usage.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = Text::default();
    for (index, entry) in COMMANDS.iter().enumerate() {
        if index > 0 {
            text.push(Style::Plain, "\n");
        }
        text.push(Style::Cyan, entry.usage);
        text.push(
            Style::Plain,
            format!(
                "{}{}",
                " ".repeat(width - entry.usage.chars().count() + 2),
                entry.description
            ),
        );
    }
    text
}

/// Match `line` against the registry, longest name first, so `/config model`
/// wins over `/config` and `/models` is not read as `/model` plus `s`.
pub fn find_command_for_line(line: &str) -> Option<(&'static CommandEntry, String)> {
    let mut entries: Vec<&'static CommandEntry> = COMMANDS.iter().collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.name.len()));
    entries.into_iter().find_map(|entry| {
        if line == entry.name {
            Some((entry, String::new()))
        } else {
            line.strip_prefix(entry.name)
                .and_then(|rest| rest.strip_prefix(' '))
                .map(|arg| (entry, arg.trim().to_owned()))
        }
    })
}

/// Where a provider's key comes from, never the key itself.
fn key_source(name: &str, api_key_env: Option<&str>, secrets: &dyn SecretStore) -> (Style, String) {
    let Some(variable) = api_key_env.filter(|variable| !variable.is_empty()) else {
        return (Style::Dim, "n/a".into());
    };
    if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
        (Style::Green, "env".into())
    } else if secrets.get_password(SERVICE_NAME, name).is_some() {
        (Style::Green, "keyring".into())
    } else {
        (Style::Yellow, "none".into())
    }
}

/// `/models`: the providers and where their keys come from, then the model roles.
pub fn models_tables(config: &Config, secrets: &dyn SecretStore) -> Text {
    let dash = || (Style::Dim, "-".to_owned());
    let providers: Vec<Vec<(Style, String)>> = config
        .providers
        .iter()
        .map(|(name, provider)| {
            vec![
                (Style::Plain, name.clone()),
                provider
                    .base_url
                    .clone()
                    .map_or_else(dash, |url| (Style::Plain, url)),
                provider
                    .api_key_env
                    .clone()
                    .map_or_else(dash, |env| (Style::Plain, env)),
                key_source(name, provider.api_key_env.as_deref(), secrets),
            ]
        })
        .collect();
    let roles: Vec<Vec<(Style, String)>> = [
        ("default", &config.model.default),
        ("route.cheap", &config.model.route.cheap),
        ("route.frontier", &config.model.route.frontier),
    ]
    .into_iter()
    .map(|(role, reference)| {
        let valid = if ModelRef::parse(reference).is_ok() {
            (Style::Green, "ok".to_owned())
        } else {
            (Style::Red, "invalid".to_owned())
        };
        vec![
            (Style::Plain, role.to_owned()),
            (Style::Plain, reference.clone()),
            valid,
        ]
    })
    .collect();
    let mut text = table(
        "Providers",
        &["Provider", "Base URL", "API key env", "Key"],
        &providers,
    );
    text.push(Style::Plain, "\n");
    text.append(table("Model roles", &["Role", "Model", "Valid"], &roles));
    text.push(Style::Plain, "\nOS keyring: ");
    text.push(Style::Green, "available");
    text
}

/// `when` (seconds since the epoch) as a coarse "how long ago", as Python's `relative_time`.
pub fn relative_time(when: f64, now: f64) -> String {
    if when == 0.0 {
        return "unknown".into();
    }
    let delta = now - when;
    if delta < 60.0 {
        // Covers a clock that skewed backwards, too.
        "just now".into()
    } else if delta < 3600.0 {
        format!("{}m ago", (delta / 60.0) as u64)
    } else if delta < 86400.0 {
        format!("{}h ago", (delta / 3600.0) as u64)
    } else if delta < 86400.0 * 30.0 {
        format!("{}d ago", (delta / 86400.0) as u64)
    } else {
        format!("{}mo ago", (delta / (86400.0 * 30.0)) as u64)
    }
}

/// One session as a single line, `title · id[:8] · how long ago`, as Python's
/// `session_label`. Shared by the session picker and anything else that lists
/// sessions on one line. The title is the user's own text, so it is drawn plain.
pub fn session_label(info: &SessionInfo, now: f64) -> String {
    format!(
        "{} · {} · {}",
        info.label(),
        short_id(&info.session_id),
        relative_time(info.updated_at, now)
    )
}

/// The model picker's choices, as Python's `model_choices`: the three role slots,
/// deduplicated by ref, with roles that share a ref named together
/// (`default, route.frontier — openai:gpt-4o`), then `extra`, the active
/// provider's catalog as (ref, label) with full refs, minus what is already listed.
/// Each choice is (label, ref).
pub fn model_choices(config: &Config, extra: &[(String, String)]) -> Vec<(String, String)> {
    let roles = [
        ("default", &config.model.default),
        ("route.cheap", &config.model.route.cheap),
        ("route.frontier", &config.model.route.frontier),
    ];
    let mut order: Vec<(&str, Vec<&str>)> = Vec::new();
    for (role, reference) in roles {
        if reference.is_empty() {
            continue;
        }
        match order
            .iter_mut()
            .find(|(seen, _)| *seen == reference.as_str())
        {
            Some((_, names)) => names.push(role),
            None => order.push((reference, vec![role])),
        }
    }
    let mut choices: Vec<(String, String)> = order
        .into_iter()
        .map(|(reference, names)| {
            (
                format!("{} — {reference}", names.join(", ")),
                reference.to_owned(),
            )
        })
        .collect();
    for (reference, label) in extra {
        if !choices.iter().any(|(_, listed)| listed == reference) {
            choices.push((label.clone(), reference.clone()));
        }
    }
    choices
}

pub(crate) fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn short_id(session_id: &str) -> &str {
    session_id
        .char_indices()
        .nth(8)
        .map_or(session_id, |(end, _)| &session_id[..end])
}

// --------------------------------------------------------------------------
// The REPL
// --------------------------------------------------------------------------

/// The interactive session: the harness, the live conversation, and the
/// prompt history, as Python's `_Repl`.
pub struct Repl {
    pub config: Config,
    pub harness: Harness,
    pub store: Option<SessionStore>,
    pub state: AgentState,
    /// The current model's `provider:model` ref, which usage is priced under.
    model_ref: String,
    prices: PriceTable,
    /// Where `/models` looks for stored keys.
    pub(crate) secrets: Box<dyn SecretStore>,
    /// Built on first use, so constructing a `Repl` never reads the history file.
    history: Option<PromptHistory>,
    /// How far the MCP note log has been shown.
    pub(crate) mcp_cursor: u64,
    /// Launch notes: resume, memory, MCP, collisions, sandbox, and the session id.
    pub startup_notes: Vec<String>,
    /// Where the configuration came from, so the interface can write it back and
    /// reload it; `None` when there is nowhere to write (tests).
    pub config_paths: Option<ConfigPaths>,
    /// Counts successful `/model` switches. Python tells a switch from a refusal by
    /// the provider object changing; this is the same signal.
    pub(crate) model_switches: u64,
}

impl Repl {
    /// Build the harness and open (or resume) the session.
    pub fn new(
        config: Config,
        resume: Option<&str>,
        prompt: Option<ApprovalPrompt>,
        secrets: Box<dyn SecretStore>,
    ) -> Result<Self, SetupError> {
        let assembly = assemble(&config, prompt)?;
        let mut resume_notes = Vec::new();
        let resumed = resume.and_then(|id| {
            let loaded = assembly
                .store
                .as_ref()
                .map(|store| store.load(id))
                .transpose()
                .unwrap_or_else(|error| {
                    resume_notes.push(error.to_string());
                    None
                })
                .flatten();
            match &loaded {
                None => resume_notes.push(format!("no session '{id}' found; starting fresh")),
                Some(state) => {
                    // The title is what tells the user which conversation they walked back into.
                    let note = format!(
                        "resumed session {} ({} messages)",
                        state.session_id,
                        state.messages.len()
                    );
                    let title = assembly
                        .store
                        .as_ref()
                        .and_then(|store| store.title(&state.session_id).ok().flatten());
                    resume_notes.push(match title {
                        Some(title) => format!("{note} - {title}"),
                        None => note,
                    });
                }
            }
            loaded
        });
        let mut repl = Self {
            prices: PriceTable::from_config(&config),
            model_ref: assembly.model_ref.to_string(),
            harness: assembly.harness,
            store: assembly.store,
            state: AgentState::new(),
            config,
            secrets,
            history: None,
            mcp_cursor: 0,
            startup_notes: Vec::new(),
            config_paths: None,
            model_switches: 0,
        };
        repl.state = match resumed {
            Some(state) => state,
            None => repl.new_session(),
        };
        // Marked before the startup notes are collected, so a note racing startup
        // can be shown twice but never lost.
        repl.mcp_cursor = repl
            .harness
            .mcp
            .as_ref()
            .map_or(0, |mcp| mcp.notes_since(u64::MAX).0);
        let mut notes = resume_notes;
        if let Some(note) = &assembly.memory_note {
            notes.push(format!("memory: {note}"));
        }
        if let Some(mcp) = &repl.harness.mcp {
            notes.extend(mcp.notes().iter().map(|note| format!("mcp: {note}")));
        }
        let collisions = repl.harness.registry.collisions();
        if !collisions.is_empty() {
            notes.push(format!(
                "tools: name collision, not callable: {}",
                collisions.join(", ")
            ));
        }
        if let Some(note) = &assembly.sandbox_note {
            notes.push(format!("sandbox: {note}"));
        }
        notes.push(format!("session {}", repl.state.session_id));
        repl.startup_notes = notes;
        Ok(repl)
    }

    /// A fresh session, saved at once, as Python's `Harness.new_session`.
    fn new_session(&self) -> AgentState {
        let state = AgentState::new();
        self.persist(&state);
        state
    }

    /// Save the session; a failure is not fatal (the conversation goes on unsaved).
    pub(crate) fn persist(&self, state: &AgentState) {
        if let Some(store) = &self.store {
            let _ = store.persist(state);
        }
    }

    /// Whether the current model has a known price, so a dollar figure means something.
    pub(crate) fn priced(&self) -> bool {
        self.prices.is_priced(&self.model_ref)
    }

    /// The current model's `provider:model` ref.
    pub(crate) fn model_ref(&self) -> &str {
        &self.model_ref
    }

    /// The prompt history both surfaces record into (`[ui] history_file`).
    pub fn history(&mut self) -> &mut PromptHistory {
        let ui = &self.config.ui;
        self.history
            .get_or_insert_with(|| PromptHistory::from_config(ui))
    }

    /// The live MCP manager, made (empty, and attached to the tool registry) when
    /// the session started without one, so the first server can be added mid-session
    /// (Python's `ensure_mcp`).
    pub fn ensure_mcp(&mut self) -> std::sync::Arc<crate::mcp::McpManager> {
        if let Some(mcp) = &self.harness.mcp {
            return std::sync::Arc::clone(mcp);
        }
        let mcp = std::sync::Arc::new(crate::mcp::McpManager::empty(&self.config));
        mcp.attach(&self.harness.registry);
        self.mcp_cursor = 0;
        self.harness.mcp = Some(std::sync::Arc::clone(&mcp));
        mcp
    }

    /// MCP notes recorded since the last call, `mcp: `-prefixed. A repeat outage is
    /// shown again: a second identical drop is real news, not noise.
    pub fn pending_mcp_notes(&mut self) -> Vec<String> {
        let Some(mcp) = &self.harness.mcp else {
            return Vec::new();
        };
        let (seq, fresh) = mcp.notes_since(self.mcp_cursor);
        self.mcp_cursor = seq;
        fresh
            .into_iter()
            .map(|note| format!("mcp: {note}"))
            .collect()
    }

    /// Dispatch one stripped input line, as Python's `handle_command`.
    pub fn handle_command(&mut self, line: &str) -> CommandResult {
        if line.is_empty() {
            return CommandResult::of(Action::Noop);
        }
        if line == "/exit" || line == "/quit" {
            return CommandResult::of(Action::Exit);
        }
        if !line.starts_with('/') {
            return CommandResult::of(Action::Turn);
        }
        match find_command_for_line(line) {
            Some((entry, arg)) => (entry.handler)(self, &arg),
            None => CommandResult::message(format!("unknown command {line}")),
        }
    }

    /// The one-line summary of the turn that just finished.
    pub fn turn_summary_line(&self) -> Option<String> {
        self.state
            .last_turn
            .as_ref()
            .map(|turn| format_turn_summary(turn, self.priced()))
    }

    fn config_render(&self) -> String {
        serde_json::to_value(&self.config.table)
            .and_then(|value| serde_json::to_string_pretty(&value))
            .unwrap_or_else(|error| format!("could not render the configuration: {error}"))
    }

    fn set_model(&mut self, arg: &str) -> String {
        if arg.is_empty() {
            return format!(
                "current model: {}:{}\nusage: /model <provider:model>  \
                 e.g. /model openrouter:anthropic/claude-3.7-sonnet",
                self.harness.provider.name(),
                self.harness.provider.model()
            );
        }
        match build_provider(&self.config, arg) {
            // The current model is left unchanged.
            Err(message) => message,
            Ok((provider, reference)) => {
                self.harness.max_tokens = self.config.max_output_tokens_for(&reference.provider);
                self.harness.context_window =
                    crate::setup::model_context_window(&self.config, &reference.to_string());
                self.harness.provider = provider;
                self.model_ref = reference.to_string();
                self.model_switches += 1;
                format!(
                    "model: {}:{}",
                    self.harness.provider.name(),
                    self.harness.provider.model()
                )
            }
        }
    }

    fn set_mode(&mut self, arg: &str) -> String {
        let gate = &self.harness.gate;
        if arg.is_empty() {
            gate.set_mode(gate.mode().cycle());
        } else {
            match ApprovalMode::parse(arg) {
                Some(mode) => gate.set_mode(mode),
                None => return format!("unknown mode '{arg}' (ask|auto|deny)"),
            }
        }
        format!("approval mode: {}", gate.mode().as_str())
    }

    fn tools_render(&self) -> String {
        let mut lines = vec![format!(
            "tools: {}",
            self.harness.registry.names().join(", ")
        )];
        if let Some(mcp) = &self.harness.mcp {
            lines.extend(
                mcp.server_statuses()
                    .into_iter()
                    .map(|(name, status)| format!("mcp {name}: {status}")),
            );
        }
        lines.join("\n")
    }

    fn memory_render(&self) -> String {
        match self.store.as_ref().map(SessionStore::counts) {
            Some(Ok(counts)) => format!(
                "memory sessions {}  episodes {}  facts {}  session {}",
                counts.sessions, counts.episodes, counts.memories, self.state.session_id
            ),
            Some(Err(error)) => error.to_string(),
            None => "memory is disabled".into(),
        }
    }

    fn cost_render(&self) -> String {
        let usage = &self.state.usage;
        // On a model with no known rate, a running total of $0.0000 would read as
        // "free" when the truth is "unknown"; the label says so itself.
        let cost = format_cost(usage.cost_usd, self.priced());
        let cost_text = if cost == UNPRICED_LABEL {
            cost
        } else {
            format!("cost {cost}")
        };
        let mut line = format!(
            "steps {}  in {}  out {} tok  {cost_text}",
            self.state.step_count(),
            usage.input_tokens,
            usage.output_tokens
        );
        if usage.cache_read_tokens > 0 || usage.cache_creation_tokens > 0 {
            line.push_str(&format!(
                "\ncache read {}  written {} tok",
                usage.cache_read_tokens, usage.cache_creation_tokens
            ));
        }
        line
    }

    /// Recent sessions, or `None` when memory (and so the log) is off. A broken
    /// log must not take a command down, so a failure reads as no sessions.
    pub(crate) fn recent_sessions(&self, limit: usize) -> Option<Vec<SessionInfo>> {
        self.store
            .as_ref()
            .map(|store| store.recent_sessions(limit).unwrap_or_default())
    }

    fn sessions_render(&self, now: f64) -> Text {
        let Some(sessions) = self.recent_sessions(20) else {
            return Text::plain(NO_MEMORY_NOTE);
        };
        if sessions.is_empty() {
            return Text::plain("no saved sessions yet");
        }
        let rows: Vec<Vec<(Style, String)>> = sessions
            .iter()
            .map(|info| {
                let active = info.session_id == self.state.session_id;
                vec![
                    (
                        if active { Style::Green } else { Style::Plain },
                        short_id(&info.session_id).to_owned(),
                    ),
                    (Style::Plain, info.label().to_owned()),
                    (Style::Plain, relative_time(info.updated_at, now)),
                ]
            })
            .collect();
        table("Recent sessions", &["Session", "Title", "Updated"], &rows)
    }

    /// Turn what the user typed into a full session id: an exact id, or a prefix
    /// (the lists show eight characters) that matches exactly one session.
    fn resolve_session_id(&self, arg: &str) -> Option<String> {
        let ids: Vec<String> = self
            .recent_sessions(200)
            .unwrap_or_default()
            .into_iter()
            .map(|info| info.session_id)
            .collect();
        if ids.iter().any(|id| id == arg) {
            return Some(arg.to_owned());
        }
        let mut matches = ids.into_iter().filter(|id| id.starts_with(arg));
        match (matches.next(), matches.next()) {
            (Some(id), None) => Some(id),
            _ => None,
        }
    }

    fn resume(&mut self, arg: &str) -> CommandResult {
        let Some(store) = &self.store else {
            return CommandResult::message(NO_MEMORY_NOTE);
        };
        if arg.is_empty() {
            // The plain loop shows the list, so an id can be copied out of it.
            return CommandResult::handled(self.sessions_render(now()));
        }
        let loaded = self
            .resolve_session_id(arg)
            .and_then(|id| store.load(&id).ok().flatten());
        let Some(state) = loaded else {
            return CommandResult::message(format!("no session '{arg}' found"));
        };
        let message = self.resumed_note(&state);
        self.state = state;
        CommandResult::message(message)
    }

    /// Make the picked session the live one, as Python's `_on_session_picked`:
    /// the note a rebuilt transcript ends with, or why the session could not load.
    pub(crate) fn switch_session(&mut self, session_id: &str) -> Result<String, String> {
        let loaded = self
            .store
            .as_ref()
            .and_then(|store| store.load(session_id).ok().flatten());
        let Some(state) = loaded else {
            return Err(format!("no session '{session_id}' found"));
        };
        let note = self.resumed_note(&state);
        self.state = state;
        Ok(note)
    }

    /// `resumed session <id> (<n> messages) - <title>`: an id alone doesn't say
    /// which conversation this is, so the stored title comes along.
    fn resumed_note(&self, state: &AgentState) -> String {
        let title = self
            .store
            .as_ref()
            .and_then(|store| store.title(&state.session_id).ok().flatten());
        let note = format!(
            "resumed session {} ({} messages)",
            state.session_id,
            state.messages.len()
        );
        match title {
            Some(title) => format!("{note} - {title}"),
            None => note,
        }
    }
}

// --------------------------------------------------------------------------
// Terminal input
// --------------------------------------------------------------------------

/// One thing that happened at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    Line(String),
    Eof,
    Interrupt,
}

/// Lines from stdin, read on demand by a helper thread, and Ctrl+C presses,
/// reported by a signal thread that also cancels the running turn.
///
/// Reading only on demand matters: during a turn no read is pending, so the
/// approval prompt can read stdin itself.
struct TerminalInput {
    events: mpsc::Receiver<Input>,
    requests: mpsc::Sender<()>,
    pending: bool,
    stashed: Option<Input>,
}

impl TerminalInput {
    fn start(harness: &Harness) -> Self {
        let (event_tx, events) = mpsc::channel();
        let (requests, request_rx) = mpsc::channel::<()>();
        let lines = event_tx.clone();
        std::thread::spawn(move || {
            for () in request_rx {
                let mut buffer = Vec::new();
                let event = match std::io::stdin().lock().read_until(b'\n', &mut buffer) {
                    Ok(0) | Err(_) => Input::Eof,
                    Ok(_) => {
                        if buffer.ends_with(b"\n") {
                            buffer.pop();
                            if buffer.ends_with(b"\r") {
                                buffer.pop();
                            }
                        }
                        Input::Line(String::from_utf8_lossy(&buffer).into_owned())
                    }
                };
                if lines.send(event).is_err() {
                    break;
                }
            }
        });
        let cancel = harness.cancel.clone();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                while tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                    if event_tx.send(Input::Interrupt).is_err() {
                        break;
                    }
                }
            });
        });
        Self {
            events,
            requests,
            pending: false,
            stashed: None,
        }
    }

    /// The next line, end of input, or a Ctrl+C. After a Ctrl+C the read stays
    /// pending, so the next call waits for the same line.
    fn next(&mut self) -> Input {
        if let Some(event) = self.stashed.take() {
            return event;
        }
        if !self.pending {
            if self.requests.send(()).is_err() {
                return Input::Eof;
            }
            self.pending = true;
        }
        let event = self.events.recv().unwrap_or(Input::Eof);
        if event != Input::Interrupt {
            self.pending = false;
        }
        event
    }

    /// Forget Ctrl+C presses that landed during a turn; they cancelled it already.
    fn drain_interrupts(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if event != Input::Interrupt {
                self.pending = false;
                self.stashed = Some(event);
            }
        }
    }
}

// --------------------------------------------------------------------------
// The loop
// --------------------------------------------------------------------------

/// stdout, styled like Rich: colour only on a terminal, and never with `NO_COLOR`.
struct Console {
    color: bool,
}

impl Console {
    fn new() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self {
            color: std::io::stdout().is_terminal() && !no_color,
        }
    }

    fn write(&self, text: &Text) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.render(self.color).as_bytes());
        let _ = stdout.flush();
    }

    fn print(&self, text: &Text) {
        let mut line = text.clone();
        line.push(Style::Plain, "\n");
        self.write(&line);
    }

    fn line(&self, style: Style, text: &str) {
        self.print(&Text::styled(style, text));
    }
}

/// The plain loop's entry point: build the session, then run the loop on it.
pub fn run_repl(config: Config, resume: Option<&str>, secrets: Box<dyn SecretStore>) {
    if let Some(repl) = start_repl(config, resume, secrets) {
        plain_loop(repl);
    }
}

/// Build the session both surfaces run on, or say why it cannot start.
///
/// Approvals are asked at the terminal; piped input has nobody to ask, so `ask`
/// declines. The full-screen interface replaces the prompt with its own panel.
pub fn start_repl(
    config: Config,
    resume: Option<&str>,
    secrets: Box<dyn SecretStore>,
) -> Option<Repl> {
    let prompt = std::io::stdin()
        .is_terminal()
        .then(|| Box::new(stdout_prompt) as ApprovalPrompt);
    match Repl::new(config, resume, prompt, secrets) {
        Ok(repl) => Some(repl),
        Err(SetupError::Model(message) | SetupError::Workspace(message)) => {
            let console = Console::new();
            let mut text = Text::styled(Style::Red, "Cannot start:");
            text.push(Style::Plain, format!(" {message}"));
            console.print(&text);
            console.line(Style::Dim, MODEL_HELP);
            None
        }
    }
}

/// Python's plain `input()` loop over an already-built session.
pub fn plain_loop(mut repl: Repl) {
    let console = Console::new();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            console.line(
                Style::Red,
                &format!("could not start the async runtime: {error}"),
            );
            return;
        }
    };

    // A non-terminal stdout means output is piped or redirected: no decorative panel.
    if std::io::stdout().is_terminal() {
        console.print(&banner(&repl.config, env!("CARGO_PKG_VERSION")));
    }
    for note in &repl.startup_notes {
        let style = if note.starts_with("sandbox:") {
            Style::Yellow
        } else {
            Style::Dim
        };
        console.line(style, note);
    }

    let mut input = TerminalInput::start(&repl.harness);
    loop {
        for note in repl.pending_mcp_notes() {
            console.line(Style::Yellow, &note);
        }
        console.write(&Text::plain("agent86> "));
        let raw = match input.next() {
            Input::Line(raw) => raw,
            Input::Eof => {
                console.write(&Text::plain("\n"));
                console.line(Style::Dim, "bye");
                return;
            }
            Input::Interrupt => {
                console.write(&Text::plain("\n"));
                continue;
            }
        };

        let line = raw.trim();
        // The unstripped line, so a prompt typed with a leading space stays out of the file.
        repl.history().append(raw.trim_end());
        let result = repl.handle_command(line);
        match result.action {
            Action::Exit => {
                console.line(Style::Dim, "bye");
                return;
            }
            Action::Noop => continue,
            Action::Handled => {
                if let Some(render) = &result.render {
                    console.print(render);
                }
                continue;
            }
            Action::Turn => {}
        }

        // `@path` mentions become inline file blocks before the model sees the line.
        // Refusals are shown here and carried in the prompt, so neither side assumes
        // a file arrived when it didn't.
        let mentions = expand_mentions(
            line,
            &repl.harness.context.policy,
            repl.config.tools.mention_max_bytes,
        );
        for problem in &mentions.errors {
            console.line(Style::Yellow, problem);
        }

        console.write(&Text::plain("\n"));
        let mut label = Text::styled(Style::BoldCyan, "agent86");
        label.push(Style::Plain, " ");
        console.write(&label);
        let mut printed = false;
        // The label was printed without a newline: the cursor is mid-line.
        let mut at_line_start = false;
        let result = {
            let Repl { harness, state, .. } = &mut repl;
            runtime.block_on(harness.run_turn_as(state, &mentions.prompt, line, |text| {
                if text.is_empty() {
                    return;
                }
                if let Some(notice) = notice_text(text) {
                    // Harness chatter, not the answer: break the current line first.
                    if !at_line_start {
                        console.write(&Text::plain("\n"));
                        at_line_start = true;
                    }
                    console.line(Style::Dim, notice);
                    return;
                }
                console.write(&Text::plain(text));
                at_line_start = text.ends_with('\n');
                printed = true;
            }))
        };
        input.drain_interrupts();
        match result {
            Ok(_) => {
                if !printed {
                    console.write(&Text::styled(Style::Dim, "(no response)"));
                }
                console.write(&Text::plain("\n"));
            }
            Err(error) => {
                let mut text = Text::plain("\n");
                text.push(Style::Red, "error:");
                text.push(Style::Plain, format!(" {error}"));
                console.print(&text);
            }
        }
        // Saved whether the turn finished, failed, or was cancelled.
        repl.persist(&repl.state);
        // Printed on the error and cancel paths too: a turn that failed halfway still spent tokens.
        if let Some(summary) = repl.turn_summary_line() {
            console.line(Style::Dim, &summary);
        }
        console.write(&Text::plain("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_match_longest_name_first() {
        let name = |line: &str| find_command_for_line(line).map(|(entry, arg)| (entry.name, arg));
        assert_eq!(
            name("/config model"),
            Some(("/config model", String::new()))
        );
        assert_eq!(name("/config"), Some(("/config", String::new())));
        assert_eq!(name("/models"), Some(("/models", String::new())));
        assert_eq!(name("/model  a:b "), Some(("/model", "a:b".to_owned())));
        assert_eq!(name("/modelx"), None);
        assert_eq!(name("/mode auto"), Some(("/mode", "auto".to_owned())));
    }

    #[test]
    fn notices_are_recognised_by_prefix() {
        assert_eq!(
            notice_text("\n[continuation 1/3]\n"),
            Some("[continuation 1/3]")
        );
        assert_eq!(notice_text("[tool] x -> ok"), None);
        assert_eq!(notice_text("\n[cancelled]\n"), None);
    }

    #[test]
    fn relative_times_are_coarse() {
        let now = 1_000_000_000.0;
        assert_eq!(relative_time(0.0, now), "unknown");
        assert_eq!(relative_time(now + 5.0, now), "just now");
        assert_eq!(relative_time(now - 59.0, now), "just now");
        assert_eq!(relative_time(now - 125.0, now), "2m ago");
        assert_eq!(relative_time(now - 7200.0, now), "2h ago");
        assert_eq!(relative_time(now - 86400.0 * 3.0, now), "3d ago");
        assert_eq!(relative_time(now - 86400.0 * 65.0, now), "2mo ago");
    }

    #[test]
    fn bare_commands_that_need_a_choice_are_python_s() {
        let choices: Vec<(&str, Choice)> = COMMANDS
            .iter()
            .filter_map(|entry| entry.needs_choice.map(|choice| (entry.name, choice)))
            .collect();
        assert_eq!(
            choices,
            [
                ("/config model", Choice::ConfigModel),
                ("/config mcp", Choice::ConfigMcp),
                ("/model", Choice::Model),
                ("/mode", Choice::Mode),
                ("/resume", Choice::Resume),
            ]
        );
    }

    #[test]
    fn model_choices_merge_roles_that_share_a_ref() {
        let mut config = crate::config::load_config(
            &crate::config::ConfigPaths {
                user: None,
                project: std::path::PathBuf::from("does-not-exist.toml"),
            },
            &crate::config::Overrides::default(),
        )
        .unwrap();
        config.model.default = "openai:gpt-4o".into();
        config.model.route.cheap = "ollama:llama3.1".into();
        config.model.route.frontier = "openai:gpt-4o".into();
        assert_eq!(
            model_choices(&config, &[]),
            [
                (
                    "default, route.frontier — openai:gpt-4o".to_owned(),
                    "openai:gpt-4o".to_owned()
                ),
                (
                    "route.cheap — ollama:llama3.1".to_owned(),
                    "ollama:llama3.1".to_owned()
                ),
            ]
        );
        // Catalog entries follow, without repeating a role slot's ref.
        let catalog = [
            ("openai:gpt-4o".to_owned(), "openai:gpt-4o".to_owned()),
            ("openai:o3".to_owned(), "o3 (reasoning)".to_owned()),
        ];
        let choices = model_choices(&config, &catalog);
        assert_eq!(choices.len(), 3);
        assert_eq!(
            choices[2],
            ("o3 (reasoning)".to_owned(), "openai:o3".to_owned())
        );
        config.model.default.clear();
        config.model.route.cheap.clear();
        config.model.route.frontier.clear();
        assert!(model_choices(&config, &[]).is_empty());
    }

    #[test]
    fn a_session_label_is_title_id_and_age() {
        let info = SessionInfo {
            session_id: "0123456789abcdef".into(),
            title: Some("fix the parser".into()),
            updated_at: 1_000.0,
        };
        assert_eq!(
            session_label(&info, 1_000.0 + 7_200.0),
            "fix the parser · 01234567 · 2h ago"
        );
    }

    #[test]
    fn the_help_table_lists_every_command_aligned() {
        let help = help_table().plain_text();
        let lines: Vec<&str> = help.lines().collect();
        assert_eq!(lines.len(), COMMANDS.len());
        // Rendered by Rich from Python's `_help_table` (trailing padding trimmed).
        assert_eq!(lines[0], "/help                    Show this help");
        assert_eq!(
            lines[9],
            "/mode [ask|auto|deny]    Show/set approval mode (Shift+Tab cycles)"
        );
    }

    #[test]
    fn styling_is_only_added_with_color() {
        let mut text = Text::styled(Style::Red, "error:");
        text.push(Style::Plain, " boom");
        assert_eq!(text.render(false), "error: boom");
        assert_eq!(text.render(true), "\x1b[31merror:\x1b[0m boom");
    }

    struct FakeKeyring;

    impl SecretStore for FakeKeyring {
        fn get_password(&self, _service: &str, account: &str) -> Option<String> {
            (account == "groq").then(|| "stored".to_owned())
        }
    }

    #[test]
    fn models_report_where_each_key_comes_from_without_the_key() {
        let config = crate::config::load_config(
            &crate::config::ConfigPaths {
                user: None,
                project: std::path::PathBuf::from("does-not-exist.toml"),
            },
            &crate::config::Overrides::default(),
        )
        .unwrap();
        let text = models_tables(&config, &FakeKeyring).plain_text();
        let row = |name: &str| -> Vec<String> {
            text.lines()
                .find(|line| line.starts_with(name))
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(
            row("groq"),
            [
                "groq",
                "https://api.groq.com/openai/v1",
                "GROQ_API_KEY",
                "keyring"
            ]
        );
        assert_eq!(
            row("ollama"),
            ["ollama", "http://localhost:11434", "-", "n/a"]
        );
        assert_eq!(row("route.cheap"), ["route.cheap", "ollama:llama3.1", "ok"]);
        assert!(!text.contains("stored"));
    }

    #[test]
    fn the_banner_is_a_rounded_panel() {
        // Rendered by Rich from Python's `banner(cfg, compact=False)`.
        let config = crate::config::load_config(
            &crate::config::ConfigPaths {
                user: None,
                project: std::path::PathBuf::from("does-not-exist.toml"),
            },
            &crate::config::Overrides::default(),
        )
        .unwrap();
        let text = banner(&config, "1.0.0").plain_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "╭──────────────────── agent86 ────────────────────╮"
        );
        assert_eq!(
            lines[1],
            "│ version  1.0.0                                  │"
        );
        assert_eq!(
            lines[2],
            "│ model    anthropic:claude-opus-4-8   router off │"
        );
        assert_eq!(
            lines[3],
            "│ sandbox  subprocess   approval ask              │"
        );
        assert_eq!(
            lines[4],
            "│ Type /help for commands, /exit to quit.         │"
        );
        assert_eq!(
            lines[5],
            "╰─────────────────────────────────────────────────╯"
        );
    }
}
