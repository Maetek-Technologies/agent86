//! The ACP [`Backend`] over agent86's own harness: each session is a harness built
//! by [`assemble_in`] for the session's directory, a saved [`AgentState`], and the
//! memory database's session store, so an ACP session is an ordinary agent86
//! session (`agent86 run --session <id>` can continue it).
//!
//! Nothing here writes to stdout; the protocol owns it. Notes for the user go to
//! stderr.
//!
//! Tool calls are reported to the editor as they happen. A call the approval mode
//! asks about becomes a `session/request_permission` request, through the gate's
//! per-call prompt: `ask` mode asks for every side-effecting call, `auto` asks for
//! none (except a write into agent86's own configuration, which is asked about in
//! every mode but `deny`), and `deny` refuses them. The model never sees a vault
//! secret in what the editor is shown: arguments and previews are redacted first.
//!
//! A turn is cancelled through the harness's own [`CancelToken`], as Ctrl+C does in
//! the interface: it stops at the next safe point, a running tool finishes, and every
//! call in the history gets an answer. A loaded session replays its saved messages
//! as updates; the saved state keeps no per-call status or reasoning, so a replayed
//! call is shown `completed` and thoughts are not replayed.
//!
//! When the editor offers `fs/read_text_file` and `fs/write_text_file`, the file
//! tools go through them (see [`AcpFiles`]): reads see unsaved buffers, diffs are
//! made against them, and writes land in the editor, which owns the file. Anything
//! else the agent runs (a command, a Python snippet) still sees the disk.
//!
//! The MCP servers the client names are started for the session and their tools
//! registered beside the built-ins and the configured servers', behind the same
//! approvals. They run as the user, like configured ones; the editor chose them.
//! A server that fails to start is reported on stderr and the session goes on.

use super::{
    Backend, BackendError, CancelHandle, ClientCapabilities, CommandInfo, ConfigOption, FileChange,
    Location, McpSpec, MemoryReply, MemoryRequest, Notifier, Permission, PermissionRequest,
    SecretMethod, SecretReply, SessionSummary, Stop, ToolKind, TurnSink, Update,
};
use crate::agent::{Harness, HarnessError, TurnEvent};
use crate::approval::{ApprovalPreview, Asker, Question};
use crate::config::{Config, DisplayLevel, McpServerConfig};
use crate::escape::show_controls;
use crate::mcp::{var_ref_names, McpManager};
use crate::memory::{Entry, EntryKind};
use crate::pyjson::python_json_dumps;
use crate::secrets::{NoSecretStore, OsKeyring, SharedStore};
use crate::session::{AgentState, SessionStore};
use crate::session_dirs::{same_path_key, SessionDirs};
use crate::setup::{assemble_in, SetupError};
use crate::subagent::SubagentEvent;
use crate::tools::EditorFiles;
use crate::types::{ApprovalMode, Role, ToolCall, ToolResult};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long the editor has to answer a file request before the tool goes on without
/// it (a read falls back to the disk; a write fails).
const FILE_TIMEOUT: Duration = Duration::from_secs(30);

/// `/secret`: the names the session holds, or `forget NAME`. A secret is never
/// entered here: what is typed into an editor's box is shown in its thread and kept
/// in its history, and the protocol forbids asking for one through a form. So the
/// way in is a client's own masked entry, through the secrets extension, when it
/// has one (`client_entry`), and otherwise the terminal's `/secret NAME` with
/// "remember for this project", which editor sessions in that project then load.
/// Nothing typed after the command is stored or repeated back.
fn secret_command(session: &Session, arg: &str, client_entry: bool) -> String {
    let how = if client_entry {
        SECRET_HOW_CLIENT
    } else {
        SECRET_HOW
    };
    let vault = &session.harness.context.secrets;
    let words: Vec<&str> = arg.split_whitespace().collect();
    match words.as_slice() {
        [] => {
            let remembered = vault.remembered_names();
            let names: Vec<String> = vault
                .names()
                .into_iter()
                .map(|name| {
                    if remembered.contains(&name) {
                        format!("{name} (remembered)")
                    } else {
                        name
                    }
                })
                .collect();
            if names.is_empty() {
                format!("no secrets for this project · {how}")
            } else {
                format!("secrets (values hidden): {}\n{how}", names.join(", "))
            }
        }
        ["forget", name] if crate::vault::valid_name(name) => match vault.forget(name) {
            Ok(true) => format!("secret {name} forgotten"),
            Ok(false) => format!("no secret named {name}"),
            Err(error) => {
                format!("secret {name} dropped from this session, but the keyring refused: {error}")
            }
        },
        ["forget", _] => "that is not a valid secret name (letters, digits, and _)".to_owned(),
        // Not echoed: it may be the value itself.
        _ => format!("nothing was stored. {how}"),
    }
}

const SECRET_HOW: &str = "Don't type a secret here: it would stay in this thread. Enter it \
     at a terminal in this project with agent86 and /secret NAME, choose to remember it, \
     and sessions here will have it; /secret forget NAME removes one.";

/// For a client with its own masked entry (the secrets extension).
const SECRET_HOW_CLIENT: &str = "Don't type a secret here: it would stay in this thread. \
     Use this client's own secret entry, which hands it to agent86 without the thread \
     seeing it; /secret forget NAME removes one.";

/// The vault's prompt for a client with its own masked entry (beyond Python): asks
/// it with [`super::SECRET_REQUEST_METHOD`] through the running turn's sink, and
/// waits as long as the user takes. No turn, an error, a reply that is not an
/// entry, or a cancel (which drops the reply) declines.
fn client_secret_prompt(
    sink: SinkSlot,
    session_id: String,
    secrets: crate::vault::SecretVault,
) -> crate::vault::SecretPrompt {
    Box::new(move |request: &crate::vault::SecretRequest| {
        let running = sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let params = json!({
            "sessionId": session_id,
            "name": request.name,
            "reason": show_controls(&secrets.redact(&request.reason)),
            "replacing": request.replacing,
            "canRemember": request.can_remember,
            "remembered": request.remembered,
        });
        let reply = running
            .call(super::SECRET_REQUEST_METHOD, params, None)
            .ok()?;
        secret_answer(&reply)
    })
}

/// The client's answer to [`super::SECRET_REQUEST_METHOD`]: an entry with a
/// non-empty value, else `None` (declined).
fn secret_answer(reply: &Value) -> Option<crate::vault::SecretAnswer> {
    if reply.get("outcome").and_then(Value::as_str) != Some("entered") {
        return None;
    }
    let value = reply
        .get("value")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    Some(crate::vault::SecretAnswer {
        value: value.to_owned(),
        remember: reply.get("remember").and_then(Value::as_bool) == Some(true),
    })
}

/// `/thinking` and `/toolview`: show the level, or set it.
fn show_level(name: &str, level: &mut DisplayLevel, arg: &str) -> String {
    if arg.is_empty() {
        return format!("{name}: {}", level.as_str());
    }
    match DisplayLevel::parse(arg) {
        Some(new) => {
            *level = new;
            format!("{name}: {} (from the next turn)", new.as_str())
        }
        None => format!("unknown level '{arg}' (full|summary|hidden)"),
    }
}

/// Turns a turn's events into what the editor is shown, at the session's levels:
/// the terminal interface's `/thinking` and `/toolview`, beyond Python. Reasoning is
/// streamed (`full`), reduced to one `∴ thought for 12s · 2.1k chars` line when it
/// ends (`summary`), or dropped (`hidden`). A tool call shows its arguments and the
/// whole result (`full`), its name and a one-line result (`summary`), or nothing
/// unless it failed (`hidden`). Approval requests carry their own description of the
/// call, so a hidden call is still asked about.
struct Presenter {
    thinking: DisplayLevel,
    tools: DisplayLevel,
    /// When the current reasoning began, and how many characters it has run to.
    thought: Option<(std::time::Instant, usize)>,
    /// The session's tool call ids and cards, shared with the sub-agent observer.
    cards: Cards,
    /// The jail, to report the files a call touches as absolute `locations`.
    policy: Option<crate::tools::SandboxPolicy>,
    /// The client takes `compaction_update` (`session.compaction`); otherwise a
    /// compaction is the agent message it always was.
    compaction: bool,
}

/// A compaction id no other compaction in the session has: the spec requires one
/// per compaction, never reused, and the session can outlive this process.
fn compaction_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("cmp-{millis}-{n}")
}

/// How many lines of a sub-agent's log its card shows; earlier ones are counted.
const CARD_LINES: usize = 20;

/// The tool call ids this layer reports, and each delegation's card (beyond
/// Python). Provider ids are not unique within a session (Ollama's restart with
/// every response, and the OpenAI adapter falls back to `call_<n>`), so a call is
/// reported under its provider id the first time that id appears and with a
/// `#<n>` suffix after that. The model's history keeps the provider's id. Replay
/// draws from the same set, so a loaded session's new calls never reuse a
/// replayed id.
#[derive(Default)]
struct CardState {
    used: HashSet<String>,
    /// The id each provider id was last reported under.
    latest: HashMap<String, String>,
    /// The ids already announced with a `tool_call`.
    announced: HashSet<String>,
    /// The running turn's `/toolview`, which a card follows.
    tools: Option<DisplayLevel>,
    /// Each card's log, by reported id.
    logs: HashMap<String, CardLog>,
    counter: u64,
    /// The card each sub-agent run reports on, by its run number, so the state
    /// events can name the card a sub-agent's model calls belong to.
    runs: HashMap<u64, String>,
}

#[derive(Default)]
struct CardLog {
    lines: Vec<String>,
    calls: usize,
}

type Cards = Arc<Mutex<CardState>>;

impl CardState {
    /// A new id for a call the provider named `provider`.
    fn assign(&mut self, provider: &str) -> String {
        let mut id = provider.to_owned();
        while id.is_empty() || self.used.contains(&id) {
            self.counter += 1;
            id = format!("{provider}#{}", self.counter);
        }
        self.used.insert(id.clone());
        self.latest.insert(provider.to_owned(), id.clone());
        id
    }

    /// The id `provider` was last reported under, or a new one.
    fn lookup(&mut self, provider: &str) -> String {
        match self.latest.get(provider) {
            Some(id) => id.clone(),
            None => self.assign(provider),
        }
    }

    fn level(&self) -> DisplayLevel {
        self.tools.unwrap_or(DisplayLevel::Full)
    }

    /// The card's content for the editor: its last [`CARD_LINES`] lines, with a
    /// count of those dropped, or a running count of calls at `summary`.
    fn card_text(&self, id: &str) -> String {
        let Some(log) = self.logs.get(id) else {
            return String::new();
        };
        if self.level() == DisplayLevel::Summary {
            let mut text = format!(
                "{} tool call{}",
                log.calls,
                if log.calls == 1 { "" } else { "s" }
            );
            for line in log.lines.iter().filter(|line| line.starts_with('\u{2717}')) {
                text.push('\n');
                text.push_str(line);
            }
            return text;
        }
        let dropped = log.lines.len().saturating_sub(CARD_LINES);
        let mut text = String::new();
        if dropped > 0 {
            text.push_str(&format!("({dropped} earlier lines not shown)\n"));
        }
        text.push_str(&log.lines[dropped..].join("\n"));
        text
    }
}

impl Presenter {
    fn new(thinking: DisplayLevel, tools: DisplayLevel, cards: Cards) -> Self {
        Self {
            thinking,
            tools,
            thought: None,
            cards,
            policy: None,
            compaction: false,
        }
    }

    /// Report the files calls touch, resolved through `policy`.
    fn with_policy(mut self, policy: crate::tools::SandboxPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Report compactions as `compaction_update` (the client said it takes them).
    fn with_compaction(mut self, compaction: bool) -> Self {
        self.compaction = compaction;
        self
    }

    fn cards(&self) -> std::sync::MutexGuard<'_, CardState> {
        self.cards.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Say that held reasoning ended, as its summary line.
    fn end_thought(&mut self, sink: &dyn TurnSink) {
        if let Some((started, chars)) = self.thought.take() {
            sink.update(Update::Thought(crate::ui::thought_summary(
                started.elapsed(),
                chars,
            )));
        }
    }

    fn announce(
        &self,
        id: &str,
        call: &ToolCall,
        sink: &dyn TurnSink,
        secrets: &crate::vault::SecretVault,
    ) {
        self.cards().announced.insert(id.to_owned());
        sink.update(Update::ToolCall {
            id: id.to_owned(),
            title: title(call, secrets),
            name: call.name.clone(),
            kind: tool_kind(&call.name),
            raw_input: if self.tools == DisplayLevel::Full {
                redacted_input(call, secrets)
            } else {
                Value::Null
            },
            locations: call_locations(call, self.policy.as_ref(), secrets),
        });
    }

    fn event(
        &mut self,
        event: TurnEvent<'_>,
        sink: &dyn TurnSink,
        secrets: &crate::vault::SecretVault,
    ) {
        if !matches!(event, TurnEvent::Thinking(_)) {
            self.end_thought(sink);
        }
        match event {
            TurnEvent::Compacted { notice, summary } if self.compaction => {
                sink.update(Update::Compaction {
                    id: compaction_id(),
                    summary: summary.map(|summary| secrets.redact(summary)),
                    error: summary.is_none().then(|| notice.trim().to_owned()),
                })
            }
            TurnEvent::Text(chunk)
            | TurnEvent::Notice(chunk)
            | TurnEvent::Compacted { notice: chunk, .. }
                if !chunk.is_empty() =>
            {
                sink.update(Update::Message(chunk.to_owned()))
            }
            // An ACP session declares no renderer until the a2ui extension lands
            // (ADR 0005, 1.2.2), so `render_ui` is not offered there yet.
            TurnEvent::Ui(_) => {}
            TurnEvent::Thinking(chunk) if !chunk.is_empty() => match self.thinking {
                DisplayLevel::Full => sink.update(Update::Thought(chunk.to_owned())),
                DisplayLevel::Summary => {
                    let held = self
                        .thought
                        .get_or_insert_with(|| (std::time::Instant::now(), 0));
                    held.1 += chunk.chars().count();
                }
                DisplayLevel::Hidden => {}
            },
            // An id is assigned whatever the level, so a later update can find it.
            TurnEvent::ToolCall { call, .. } => {
                let id = self.cards().assign(&call.id);
                if self.tools != DisplayLevel::Hidden {
                    self.announce(&id, call, sink, secrets);
                }
            }
            TurnEvent::ToolResult {
                call,
                result,
                observation,
                ..
            } => {
                let id = self.cards().lookup(&call.id);
                let announced = self.cards().announced.contains(&id);
                if self.tools == DisplayLevel::Hidden && result.ok && !announced {
                    // Only a failure is shown, and it needs the call announced first.
                    return;
                }
                if !announced {
                    self.announce(&id, call, sink, secrets);
                }
                let text = match (self.tools, result.ok) {
                    (DisplayLevel::Full, _) | (_, false) => result_text(result, observation),
                    _ => crate::agent::summarize(result),
                };
                // The diff is what an editor reviews, so it is sent at `summary` too.
                let locations = match result_location(result) {
                    Some(location) => vec![location],
                    None => call_locations(call, self.policy.as_ref(), secrets),
                };
                sink.update(Update::ToolResult {
                    id,
                    ok: result.ok,
                    text,
                    diff: result_diff(result),
                    locations,
                });
            }
            _ => {}
        }
    }
}

/// The recorder's event kinds a client that asked is sent, as the trace holds them.
const STATE_KINDS: [&str; 3] = ["model_call", "guardrail", "route"];

/// Turns the harness's own events into state events for a client that asked for
/// them (`com.maetek.agent86/state`, beyond Python). Every event is redacted as
/// the trace is (the vault's values, then `[observability] redact`), so a client
/// never sees more than the trace would hold, whether or not the trace is written.
struct StateReporter {
    cards: Cards,
    secrets: crate::vault::SecretVault,
    redact: crate::config::RedactMode,
    max_field_chars: usize,
    prices: crate::pricing::PriceTable,
    /// The `provider:model` refs the session can run on as configured (the default,
    /// the triage routes, the specialties' own models), to price a model call whose
    /// event names only the bare model id.
    known_refs: Vec<String>,
    /// `[limits] max_cost_usd`, or `None` when it is off (`0`).
    cost_cap: Option<f64>,
    /// What the running turn has spent, sub-agents included, from its model calls.
    turn_cost: Mutex<f64>,
}

impl StateReporter {
    fn new(
        config: &Config,
        known_refs: Vec<String>,
        cards: Cards,
        secrets: crate::vault::SecretVault,
    ) -> Self {
        Self {
            cards,
            secrets,
            redact: config.observability.redact,
            max_field_chars: config.observability.max_field_chars,
            prices: crate::pricing::PriceTable::from_config(config),
            known_refs,
            cost_cap: (config.limits.max_cost_usd > 0.0).then_some(config.limits.max_cost_usd),
            turn_cost: Mutex::new(0.0),
        }
    }

    /// Whether a call's cost means anything. `model` is the bare id `model_call`
    /// records, so: a call that cost something was priced; a model priced as
    /// written is; and one of the configured refs with that model is priced or not
    /// as that ref is (a free local model included). A model reached some other way
    /// (`/model` to an unconfigured ref) that cost nothing reads as unpriced.
    fn priced(&self, model: &str, cost: f64) -> bool {
        cost > 0.0
            || self.prices.is_priced(model)
            || self.known_refs.iter().any(|reference| {
                reference.split_once(':').map(|(_, bare)| bare.trim()) == Some(model)
                    && self.prices.is_priced(reference)
            })
    }

    /// One recorder event, already redacted: forwarded if it is a kind clients are
    /// sent, a model call with what it means for the turn's spend.
    fn record(&self, sink: &dyn TurnSink, record: &Map<String, Value>) {
        let kind = record
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "turn_start" {
            *self
                .turn_cost
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = 0.0;
        }
        if !STATE_KINDS.contains(&kind) {
            return;
        }
        let mut event = record.clone();
        // The notification names the session; the recorder's own label for it
        // (`sub` for some sub-agent events) would only confuse.
        event.remove("session");
        if kind == "model_call" {
            let cost = record
                .get("cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let spent = {
                let mut total = self
                    .turn_cost
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                *total += cost;
                *total
            };
            let model = record
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            event.insert("priced".into(), json!(self.priced(model, cost)));
            event.insert("turn_cost_usd".into(), json!(spent));
            event.insert("cost_cap_usd".into(), json!(self.cost_cap));
            if let Some(run) = record.get("run").and_then(Value::as_u64) {
                let cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(card) = cards.runs.get(&run) {
                    event.insert("card".into(), json!(card));
                }
            }
        }
        sink.update(Update::State(Value::Object(event)));
    }

    /// A sub-agent starting or finishing, on the card id the editor already has.
    fn subagent(&self, sink: &dyn TurnSink, event: &SubagentEvent<'_>) {
        let fields = match event {
            SubagentEvent::Started {
                card,
                run,
                name,
                depth,
                task,
            } => vec![
                ("phase", json!("started")),
                ("card", json!(self.card(&card.id))),
                ("run", json!(run)),
                ("agent", json!(name)),
                ("depth", json!(depth)),
                // As the trace's `spawn` keeps it.
                ("task", json!(task.chars().take(200).collect::<String>())),
            ],
            SubagentEvent::Finished {
                card,
                run,
                name,
                depth,
                ok,
                usage,
            } => vec![
                ("phase", json!("finished")),
                ("card", json!(self.card(&card.id))),
                ("run", json!(run)),
                ("agent", json!(name)),
                ("depth", json!(depth)),
                ("ok", json!(ok)),
                ("input_tokens", json!(usage.input_tokens)),
                ("output_tokens", json!(usage.output_tokens)),
                ("cost_usd", json!(usage.cost_usd)),
            ],
            _ => return,
        };
        self.emit(sink, "subagent", fields);
    }

    /// The questions in the gate's queue (beyond Python): the one being asked, if
    /// any, and those waiting behind it in the order they will be asked, each on the
    /// ids the editor knows (a waiting call's id is assigned now, and is the one its
    /// permission request will name).
    fn questions(&self, sink: &dyn TurnSink, asking: Option<&Question>, waiting: &[Question]) {
        let describe = |question: &Question| {
            let mut cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
            let mut entry = Map::new();
            entry.insert("kind".into(), json!(question.kind.as_str()));
            let fields = [
                ("tool", question.tool.clone()),
                ("call", question.call.as_deref().map(|id| cards.lookup(id))),
                ("agent", question.agent.clone()),
                ("card", question.card.as_deref().map(|id| cards.lookup(id))),
                ("subject", question.subject.clone()),
            ];
            for (name, value) in fields {
                if let Some(value) = value {
                    entry.insert(name.into(), json!(value));
                }
            }
            Value::Object(entry)
        };
        let fields = vec![
            ("asking", asking.map_or(Value::Null, describe)),
            (
                "waiting",
                Value::Array(waiting.iter().map(describe).collect()),
            ),
        ];
        self.emit(sink, "questions", fields);
    }

    /// The sub-agent asking a permission request, for its `_meta`: on the card id
    /// the editor knows, redacted as the state events are.
    fn asker(&self, asker: &Asker) -> Value {
        let mut record = Map::new();
        if let Some(card) = &asker.card {
            record.insert("card".into(), json!(self.card(card)));
        }
        record.insert("run".into(), json!(asker.run));
        record.insert("agent".into(), json!(asker.agent));
        record.insert("depth".into(), json!(asker.depth));
        record.insert(
            "worktree".into(),
            asker.worktree.as_ref().map_or(
                Value::Null,
                |(path, branch)| json!({"path": path, "branch": branch}),
            ),
        );
        record.insert("steps".into(), json!(asker.steps));
        record.insert("cost_usd".into(), json!(asker.cost_usd));
        record.insert("turn_cost_usd".into(), json!(self.turn_cost()));
        record.insert("cost_cap_usd".into(), json!(self.cost_cap));
        Value::Object(self.redacted(record))
    }

    /// What the running turn has spent so far, sub-agents included.
    fn turn_cost(&self) -> f64 {
        *self
            .turn_cost
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Send an event of `kind` that is agent86's own rather than the recorder's,
    /// redacted as the recorder's are.
    fn emit(&self, sink: &dyn TurnSink, kind: &str, fields: Vec<(&str, Value)>) {
        let mut record = Map::new();
        record.insert("ts".into(), json!(crate::observability::now()));
        record.insert("kind".into(), json!(kind));
        for (name, value) in fields {
            record.insert(name.into(), value);
        }
        sink.update(Update::State(Value::Object(self.redacted(record))));
    }

    /// `record` with the vault's values, then whatever `[observability] redact`
    /// removes, taken out, as the trace would hold it.
    fn redacted(&self, record: Map<String, Value>) -> Map<String, Value> {
        let mut record = Value::Object(record);
        self.secrets.redact_value(&mut record);
        let Value::Object(record) = record else {
            unreachable!("redaction keeps an object an object");
        };
        crate::observability::redact_event(&record, self.redact, self.max_field_chars)
    }

    /// The id the editor knows a card by.
    fn card(&self, provider_id: &str) -> String {
        self.cards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lookup(provider_id)
    }
}

/// Report a sub-agent on its delegation's card (beyond Python;
/// `docs/subagents.md` section 10): `in_progress` when it starts, one line per
/// tool call at `full` (a running count at `summary`, nothing at `hidden` but
/// failures), notices as lines, and for `/agent`, which has no tool call of its
/// own, a card announced when it starts and ended when it finishes. A call the
/// gate asks about is a card of its own (announced by the approval hook), which is
/// ended here. Everything shown is redacted and has its controls made visible.
fn on_subagent(
    cards: &Cards,
    sink: &dyn TurnSink,
    secrets: &crate::vault::SecretVault,
    event: &SubagentEvent<'_>,
) {
    let mut state = cards.lock().unwrap_or_else(PoisonError::into_inner);
    let level = state.level();
    let clean = |text: &str| show_controls(&secrets.redact(text));
    let progress = |state: &mut CardState, card: &str| {
        if state.announced.contains(card) && level != DisplayLevel::Hidden {
            let text = state.card_text(card);
            sink.update(Update::ToolProgress {
                id: card.to_owned(),
                text,
            });
        }
    };
    match event {
        SubagentEvent::Started {
            card,
            run,
            name,
            depth,
            task,
        } => {
            let id = state.lookup(&card.id);
            state.runs.insert(*run, id.clone());
            if level == DisplayLevel::Hidden {
                return;
            }
            let first = task.lines().next().unwrap_or_default();
            if *depth <= 1 && !state.announced.contains(&id) {
                state.announced.insert(id.clone());
                sink.update(Update::ToolCall {
                    id: id.clone(),
                    title: clean(&format!("{name}: {first}")),
                    name: "agent".into(),
                    kind: ToolKind::Other,
                    raw_input: json!({ "agent": name, "task": secrets.redact(task) }),
                    locations: Vec::new(),
                });
            }
            let line = if *depth <= 1 {
                format!("{name} started")
            } else {
                format!("\u{2192} {name} started: {}", clean(first))
            };
            state
                .logs
                .entry(id.clone())
                .or_default()
                .lines
                .push(clean(&line));
            progress(&mut state, &id);
        }
        SubagentEvent::ToolCall { card, call } => {
            let id = state.lookup(&card.id);
            let log = state.logs.entry(id.clone()).or_default();
            log.calls += 1;
            log.lines.push(title(call, secrets));
            progress(&mut state, &id);
        }
        SubagentEvent::ToolResult { card, call, result } => {
            // A call that was asked about is a card of its own: end it.
            if let Some(own) = state.latest.get(&call.id).cloned() {
                if state.announced.contains(&own) {
                    sink.update(Update::ToolResult {
                        id: own,
                        ok: result.ok,
                        text: clean(&result_text(result, &result.content)),
                        diff: result_diff(result),
                        locations: result_location(result).into_iter().collect(),
                    });
                }
            }
            let id = state.lookup(&card.id);
            let summary = clean(&crate::agent::summarize(result));
            let line = if result.ok {
                format!("  \u{21b3} {summary}")
            } else {
                format!("\u{2717} {}: {summary}", show_controls(&call.name))
            };
            state.logs.entry(id.clone()).or_default().lines.push(line);
            if result.ok || level != DisplayLevel::Hidden {
                progress(&mut state, &id);
            }
        }
        SubagentEvent::Notice { card, text } => {
            let id = state.lookup(&card.id);
            state
                .logs
                .entry(id.clone())
                .or_default()
                .lines
                .push(clean(text));
            progress(&mut state, &id);
        }
        SubagentEvent::Finished {
            card,
            name,
            depth,
            ok,
            ..
        } => {
            let id = state.lookup(&card.id);
            if *depth > 1 {
                state
                    .logs
                    .entry(id.clone())
                    .or_default()
                    .lines
                    .push(format!("\u{2190} {} finished", show_controls(name)));
                progress(&mut state, &id);
            } else if !card.announced && state.announced.contains(&id) {
                // `/agent`'s card: its answer arrives as the reply, so the card
                // ends with its log.
                let text = state.card_text(&id);
                sink.update(Update::ToolResult {
                    id,
                    ok: *ok,
                    text,
                    diff: None,
                    locations: Vec::new(),
                });
            }
        }
    }
}

/// Model lists by provider, shared with the threads that fetch them.
type Catalogs = Arc<Mutex<BTreeMap<String, Vec<(String, String)>>>>;

/// How many characters of a call's arguments go into its title.
const TITLE_ARGS: usize = 100;

/// The running turn's sink, which the session's approval hook reads. Set for the
/// length of a turn; with none set, the hook declines.
type SinkSlot = Arc<Mutex<Option<Arc<dyn TurnSink>>>>;

struct Session {
    harness: Harness,
    /// How much reasoning and tool use the editor is shown (`/thinking`, `/toolview`),
    /// as in the terminal interface. Both start `full`: an editor folds long output
    /// itself, and it is what editor sessions showed before these commands.
    thinking: DisplayLevel,
    tool_view: DisplayLevel,
    /// The `provider:model` the session runs on: the config's default until `/model`
    /// or the editor's picker switches it. Kept as the ref the user wrote, not read
    /// back from the provider, whose own name is the adapter's (`openai` for every
    /// OpenAI-compatible provider).
    model_ref: String,
    store: Option<SessionStore>,
    state: AgentState,
    sink: SinkSlot,
    /// The title `/rename` just set, until it is announced to the editor.
    title_change: Option<String>,
    /// The tool call ids reported in this session, and each delegation's card.
    cards: Cards,
}

/// Sessions on agent86's harness, driven on one single-threaded runtime.
pub struct HarnessBackend {
    config: Config,
    /// Model lists fetched from providers, by provider, as `provider:id` refs with
    /// their labels. Filled by background threads; read when options are built.
    catalogs: Catalogs,
    /// The sessions that are open, so a list arriving late can be sent to each.
    open: Arc<Mutex<Vec<String>>>,
    notifier: Option<Notifier>,
    /// Whether the lists have been asked for yet: once, on the first session.
    lists_started: bool,
    /// Where agent86 was started: the directory an unfiltered `session/list` shows
    /// for a saved session whose own was never recorded (saved before 1.0.17, or by
    /// Python's agent86), marked as unknown. A filtered list leaves those out.
    startup_cwd: String,
    /// The database, opened on its own for `session/list` when no session has yet.
    list_store: Option<SessionStore>,
    /// The home directory the session-directory file lives under (`~/.agent86`).
    home: Option<PathBuf>,
    /// Where secrets remembered for a project are read from: the OS keyring in use,
    /// `None` in tests that must not touch it.
    secret_store: Option<SharedStore>,
    client: ClientCapabilities,
    runtime: tokio::runtime::Runtime,
    sessions: HashMap<String, Session>,
}

/// The keyring editor sessions read remembered secrets from, unless
/// `AGENT86_ACP_SECRETS` is `off`: a secret entered at a terminal with `/secret`
/// and remembered for the project is then available in that project's editor
/// sessions, as in the terminal. The only change made to it from here is
/// `/secret forget`.
pub fn default_secret_store() -> Option<SharedStore> {
    let off = std::env::var("AGENT86_ACP_SECRETS")
        .is_ok_and(|setting| matches!(setting.trim(), "off" | "none" | "0" | "false"));
    (!off).then(|| Arc::new(OsKeyring) as SharedStore)
}

impl HarnessBackend {
    /// Keep the session-directory file under `home` instead of the user's, so a test
    /// never writes to the real one.
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// The record of each session's directory (see [`crate::session_dirs`]).
    fn session_dirs(&self) -> Option<SessionDirs> {
        self.home.as_deref().map(SessionDirs::under_home)
    }

    /// A backend that reads no remembered secrets.
    pub fn new(config: Config) -> std::io::Result<Self> {
        Self::with_secret_store(config, None)
    }

    /// A backend whose sessions load the secrets remembered for their project from
    /// `secret_store` (see [`default_secret_store`]).
    pub fn with_secret_store(
        config: Config,
        secret_store: Option<SharedStore>,
    ) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            config,
            catalogs: Arc::default(),
            open: Arc::default(),
            notifier: None,
            lists_started: false,
            startup_cwd: std::env::current_dir()
                .map(|dir| crate::tools::display_path(&dir))
                .unwrap_or_default(),
            list_store: None,
            home: crate::config::home_dir(),
            secret_store,
            client: ClientCapabilities::default(),
            runtime,
            sessions: HashMap::new(),
        })
    }
}

impl HarnessBackend {
    /// Build a session's harness for `cwd`, on `resume`'s saved state when given
    /// (`not_found` when the store has none), else on a fresh one, and keep it.
    fn open(
        &mut self,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
        resume: Option<&str>,
    ) -> Result<String, BackendError> {
        // No terminal prompt: the editor is asked instead (below). Without one,
        // `request_secret` is not offered, unless the client draws its own masked
        // entry (added below, once the session has its id).
        let mut assembly = assemble_in(&self.config, cwd, None, self.secret_store.clone())
            .map_err(|error| match error {
                SetupError::Model(message) => BackendError::internal(message),
                SetupError::Workspace(message) => BackendError::invalid_params(message),
            })?;
        if let Some(note) = &assembly.memory_note {
            eprintln!("memory: {note}");
        }
        if let Some(note) = &assembly.sandbox_note {
            eprintln!("sandbox: {note}");
        }
        start_client_servers(&self.config, &mut assembly.harness, &mcp_servers);

        // Approvals go to the editor, through whichever sink the running turn has.
        let sink: SinkSlot = Arc::default();
        let cards: Cards = Arc::default();
        let secrets = assembly.harness.context.secrets.clone();
        let running = |slot: &SinkSlot| slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // agent86's state events, for a client that asked (beyond Python): the
        // recorder's events as the trace holds them, and sub-agents' lifecycles.
        let reporter = self.client.state.then(|| {
            let model = &self.config.model;
            let mut known_refs = vec![
                model.default.clone(),
                model.route.cheap.clone(),
                model.route.frontier.clone(),
            ];
            known_refs.extend(
                assembly
                    .harness
                    .context
                    .specialties
                    .found
                    .values()
                    .filter_map(|specialty| specialty.model.clone()),
            );
            Arc::new(StateReporter::new(
                &self.config,
                known_refs,
                Arc::clone(&cards),
                secrets.clone(),
            ))
        });
        let hook_reporter = reporter.clone();
        let (hook_sink, hook_cards, hook_secrets) =
            (Arc::clone(&sink), Arc::clone(&cards), secrets.clone());
        assembly.harness.gate.call_prompt = Some(Box::new(
            move |call: &ToolCall, preview: &ApprovalPreview| {
                let Some(running) = running(&hook_sink) else {
                    return false;
                };
                // A sub-agent's call was never shown: it is announced first, so the
                // question names a call the editor has.
                let title = labeled(preview, title(call, &hook_secrets));
                // The change was built from the caller's own jail (a sub-agent's
                // worktree included) and redacted with the preview.
                let locations: Vec<Location> = preview
                    .change
                    .as_ref()
                    .map(change_location)
                    .into_iter()
                    .collect();
                let id = announce_for_question(
                    &hook_cards,
                    running.as_ref(),
                    call,
                    &title,
                    &hook_secrets,
                    &locations,
                );
                let request = PermissionRequest {
                    tool_call_id: id,
                    title,
                    kind: tool_kind(&call.name),
                    raw_input: redacted_input(call, &hook_secrets),
                    text: approval_text(preview),
                    remember: None,
                    diff: preview.change.clone(),
                    locations,
                    asker: hook_reporter
                        .as_ref()
                        .zip(preview.asker.as_ref())
                        .map(|(reporter, asker)| reporter.asker(asker)),
                };
                running.ask(request) == Permission::Allowed
            },
        ));
        // A project specialty's file is approved once per version: one remembering
        // option, named for its scope.
        let (hook_sink, hook_cards, hook_secrets) =
            (Arc::clone(&sink), Arc::clone(&cards), secrets.clone());
        assembly.harness.gate.specialty_prompt = Some(Box::new(
            move |call: &ToolCall, preview: &ApprovalPreview| {
                let Some(running) = running(&hook_sink) else {
                    return false;
                };
                let name = show_controls(preview.agent.as_deref().unwrap_or("specialty"));
                let title = format!("[{name}] run the project specialty");
                let id = announce_for_question(
                    &hook_cards,
                    running.as_ref(),
                    call,
                    &title,
                    &hook_secrets,
                    &[],
                );
                let request = PermissionRequest {
                    tool_call_id: id,
                    title,
                    kind: ToolKind::Other,
                    raw_input: redacted_input(call, &hook_secrets),
                    text: approval_text(preview),
                    remember: Some(format!("Allow this version of {name}")),
                    diff: None,
                    locations: Vec::new(),
                    asker: None,
                };
                running.ask(request) == Permission::Allowed
            },
        ));
        // The questions waiting behind the one asked, for an approval inbox.
        if let Some(reporter) = &reporter {
            let (hook_sink, hook_reporter) = (Arc::clone(&sink), Arc::clone(reporter));
            assembly.harness.gate.queue.set_watcher(Some(Arc::new(
                move |asking: Option<&Question>, waiting: &[Question]| {
                    if let Some(running) = running(&hook_sink) {
                        hook_reporter.questions(running.as_ref(), asking, waiting);
                    }
                },
            )));
        }
        if let Some(reporter) = &reporter {
            let (hook_sink, hook_reporter) = (Arc::clone(&sink), Arc::clone(reporter));
            assembly.harness.recorder.set_tap(Some(Arc::new(
                move |_session: &str, record: &Map<String, Value>| {
                    if let Some(running) = running(&hook_sink) {
                        hook_reporter.record(running.as_ref(), record);
                    }
                },
            )));
        }
        // Sub-agents report on their delegation's card.
        let (hook_sink, hook_cards, hook_secrets) =
            (Arc::clone(&sink), Arc::clone(&cards), secrets.clone());
        assembly.harness.subagents.set_observer(Some(Arc::new(
            move |event: &SubagentEvent<'_>| {
                if let Some(running) = running(&hook_sink) {
                    on_subagent(&hook_cards, running.as_ref(), &hook_secrets, event);
                    if let Some(reporter) = &reporter {
                        reporter.subagent(running.as_ref(), event);
                    }
                }
            },
        )));

        let state = match resume {
            None => AgentState::new(),
            Some(id) => match assembly.store.as_ref().map(|store| store.load(id)) {
                Some(Ok(Some(state))) => state,
                Some(Err(error)) => return Err(BackendError::internal(error.to_string())),
                _ => return Err(BackendError::not_found(format!("Session not found: {id}"))),
            },
        };
        let session_id = state.session_id.clone();
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(session_id.clone());
        self.start_model_lists();
        // The file tools use the editor's files when it offers any.
        if self.client.read_text_file || self.client.write_text_file {
            assembly.harness.context.editor = Some(Arc::new(AcpFiles {
                sink: Arc::clone(&sink),
                session_id: session_id.clone(),
                read: self.client.read_text_file,
                write: self.client.write_text_file,
            }));
        }
        // A client with its own masked entry is asked for secrets, so the model may
        // ask for one (beyond Python); any other client is not offered the tool.
        if self.client.secrets {
            assembly
                .harness
                .context
                .secrets
                .set_prompt(Some(client_secret_prompt(
                    Arc::clone(&sink),
                    session_id.clone(),
                    secrets.clone(),
                )));
            assembly
                .harness
                .registry
                .register(Box::new(crate::tools::secret::RequestSecretTool))
                .map_err(|error| BackendError::internal(error.to_string()))?;
        }
        self.remember_cwd(&session_id, cwd);
        let session = Session {
            thinking: DisplayLevel::Full,
            tool_view: DisplayLevel::Full,
            model_ref: assembly.model_ref.to_string(),
            harness: assembly.harness,
            store: assembly.store,
            state,
            sink,
            title_change: None,
            cards,
        };
        persist(&session.store, &session.state);
        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }
}

/// The editor's files, reached through the running turn's sink. Between turns there
/// is no sink, so nothing is asked and the disk is used. Reads fall back to the disk
/// when the editor cannot give the text (it does not know the file, or does not
/// answer); a write the editor refuses fails the call instead, since writing the
/// disk behind its back could lose the buffer's unsaved changes.
struct AcpFiles {
    sink: SinkSlot,
    session_id: String,
    read: bool,
    write: bool,
}

impl std::fmt::Debug for AcpFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpFiles")
            .field("session_id", &self.session_id)
            .field("read", &self.read)
            .field("write", &self.write)
            .finish()
    }
}

impl AcpFiles {
    fn running(&self) -> Option<Arc<dyn TurnSink>> {
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl EditorFiles for AcpFiles {
    fn read_text(&self, path: &Path) -> Option<String> {
        if !self.read {
            return None;
        }
        let sink = self.running()?;
        let result = sink
            .call(
                "fs/read_text_file",
                serde_json::json!({ "sessionId": self.session_id, "path": path.to_str()? }),
                Some(FILE_TIMEOUT),
            )
            .ok()?;
        result
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn write_text(&self, path: &Path, content: &str) -> Option<Result<(), String>> {
        if !self.write {
            return None;
        }
        let sink = self.running()?;
        let path = path.to_str()?;
        Some(
            sink.call(
                "fs/write_text_file",
                serde_json::json!({
                    "sessionId": self.session_id,
                    "path": path,
                    "content": content
                }),
                Some(FILE_TIMEOUT),
            )
            .map(|_| ()),
        )
    }
}

/// The slash commands an editor session offers: the ones that make sense with the
/// editor owning the screen, and that act on this session alone. The rest of the
/// terminal's commands (`/clear`, `/limits`, `/update`, ...) are the editor's to
/// provide, or need a terminal. `/skill` and the skills' own commands are advertised
/// beside these but run as turns, through `prompt` (see `commands`).
const COMMANDS: [(&str, &str, Option<&str>); 11] = [
    (
        "mode",
        "Show or set the approval mode for this session",
        Some("ask, auto, or deny"),
    ),
    (
        "model",
        "Show or switch the model for this session",
        Some("provider:model"),
    ),
    (
        "thinking",
        "Show or set how much of the model's reasoning is shown",
        Some("full, summary, or hidden"),
    ),
    (
        "toolview",
        "Show or set how much of each tool call is shown",
        Some("full, summary, or hidden"),
    ),
    (
        "resume",
        "List saved sessions, or continue one in this thread",
        Some("session id (or its first characters)"),
    ),
    (
        "secret",
        "List the secrets available to the agent (never their values), or forget one",
        Some("forget NAME"),
    ),
    (
        "rename",
        "Show or set this session's name",
        Some("new title"),
    ),
    ("cost", "Show token usage and cost this session", None),
    ("tools", "List the tools the agent can use", None),
    ("skills", "List the skills, and how to run each", None),
    (
        "agents",
        "List the specialist sub-agents, or show one",
        Some("name (optional)"),
    ),
];

/// `/skill`, advertised with [`COMMANDS`] but run as a turn by `prompt`, since a
/// command answered by `run_command` has no sink: an approval could not reach the
/// editor and `session/cancel` would not be read. Only a refusal is answered there.
const SKILL_COMMAND: (&str, &str, Option<&str>) = (
    "skill",
    "Run a skill now, with an optional task",
    Some("skill name, then the task (optional)"),
);

/// `/agent`, advertised with [`COMMANDS`] but run as a turn by `prompt`, like
/// [`SKILL_COMMAND`]: a specialty's approvals must reach the editor and
/// `session/cancel` must be read while it runs. Only a refusal is answered there.
const AGENT_COMMAND: (&str, &str, Option<&str>) = (
    "agent",
    "Run a specialist sub-agent on a task now",
    Some("[--with-context] name, then the task"),
);

impl HarnessBackend {
    /// `/model <ref>`, as the terminal's: the current model stays on a refusal, and a
    /// chosen model is pinned, which stops triage routing.
    fn switch_model(&self, session: &mut Session, arg: &str) -> String {
        if arg.is_empty() {
            return format!(
                "current model: {}\nusage: /model <provider:model>",
                session.model_ref
            );
        }
        match crate::setup::build_provider(&self.config, arg) {
            Err(message) => message,
            Ok((provider, reference)) => {
                let harness = &mut session.harness;
                harness.configure_output(&self.config, &reference.provider);
                harness.context_window =
                    crate::setup::model_context_window(&self.config, &reference.to_string());
                harness.provider = provider.into();
                harness.router.pin();
                session.model_ref = reference.to_string();
                format!("model: {}", session.model_ref)
            }
        }
    }

    /// `/resume`: list the saved sessions, or continue one in this thread (as the
    /// terminal's does: the conversation becomes the model's context, and the
    /// thread's new turns are saved under the resumed session).
    fn resume(&self, session: &mut Session, arg: &str) -> String {
        let Some(store) = session.store.clone() else {
            return "memory is off, so there are no saved sessions".to_owned();
        };
        let recent = store.recent_sessions(200).unwrap_or_default();
        let short = |id: &str| id.chars().take(8).collect::<String>();
        if arg.is_empty() {
            let now = crate::repl::now();
            let lines: Vec<String> = recent
                .iter()
                .filter(|info| info.title.is_some())
                .take(20)
                .map(|info| {
                    format!(
                        "{}  {}  {}",
                        short(&info.session_id),
                        info.label(),
                        crate::repl::relative_time(info.updated_at, now)
                    )
                })
                .collect();
            return if lines.is_empty() {
                "no saved sessions yet".to_owned()
            } else {
                format!("{}\nusage: /resume <id>", lines.join("\n"))
            };
        }
        let mut matches = recent.iter().filter(|info| info.session_id == arg).chain(
            recent
                .iter()
                .filter(|info| info.session_id.starts_with(arg)),
        );
        let found = match (matches.next(), matches.next()) {
            (Some(first), Some(second)) if first.session_id != second.session_id => None,
            (Some(first), _) => Some(first.session_id.clone()),
            _ => None,
        };
        let Some(id) = found else {
            return format!("no session '{arg}' found (or '{arg}' matches more than one)");
        };
        match store.load(&id) {
            Ok(Some(state)) => {
                let title = store.title(&id).ok().flatten();
                let note = format!(
                    "resumed session {id} ({} messages){}",
                    state.messages.len(),
                    title.map(|title| format!(" - {title}")).unwrap_or_default()
                );
                self.remember_cwd(
                    &id,
                    &crate::tools::plain_path(session.harness.context.policy.workspace()),
                );
                session.state = state;
                format!("{note}\nThe earlier conversation is in the model's context; it is not shown here.")
            }
            _ => format!("no session '{arg}' found"),
        }
    }

    /// Note that `session_id` works in `cwd`, for `session/list`: the database has
    /// no such column, and it is shared with Python's. The terminal records its
    /// sessions in the same file, through the same code.
    fn remember_cwd(&self, session_id: &str, cwd: &Path) {
        if let Some(dirs) = self.session_dirs() {
            dirs.record(session_id, cwd);
        }
    }

    /// The store `session/list` reads: an open session's, else the database opened
    /// on its own (without the embedding model, which a read needs none of).
    fn store_for_listing(&mut self) -> Option<SessionStore> {
        if !self.config.memory.enabled {
            return None;
        }
        if let Some(store) = self
            .sessions
            .values()
            .find_map(|session| session.store.clone())
        {
            return Some(store);
        }
        if self.list_store.is_none() {
            self.list_store =
                SessionStore::open(&crate::session::expand_home(&self.config.memory.path)).ok();
        }
        self.list_store.clone()
    }

    /// `/cost`, as the terminal's.
    fn cost(&self, session: &Session) -> String {
        let usage = &session.state.usage;
        let harness = &session.harness;
        let reference = harness
            .routed_ref()
            .unwrap_or_else(|| session.model_ref.clone());
        let priced = crate::pricing::PriceTable::from_config(&self.config).is_priced(&reference);
        let cost = crate::pricing::format_cost(usage.cost_usd, priced);
        let cost_text = if cost == crate::pricing::UNPRICED_LABEL {
            cost
        } else {
            format!("cost {cost}")
        };
        let mut line = format!(
            "steps {}  in {}  out {} tok  {cost_text}",
            session.state.step_count(),
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
}

impl HarnessBackend {
    /// Ask each provider that can answer for its model list, in the background, once.
    /// A provider is asked when its key resolves (the environment, then the keyring),
    /// when it is Ollama, which has none, or when its key is optional (vLLM, beyond
    /// Python), once something listens at its URL. Each list that arrives is kept and every
    /// open session told, so the picker fills in after the session is already usable;
    /// a failure is a line on stderr and nothing else. `AGENT86_ACP_MODEL_LISTS`
    /// narrows it: `off` asks nobody (a picker of just the configured models), and a
    /// comma-separated list of provider names asks only those.
    fn start_model_lists(&mut self) {
        if std::mem::replace(&mut self.lists_started, true) {
            return;
        }
        let setting = std::env::var("AGENT86_ACP_MODEL_LISTS").unwrap_or_default();
        let only: Option<Vec<String>> = match setting.trim() {
            "" => None,
            "off" | "none" | "0" | "false" => return,
            names => Some(
                names
                    .split(',')
                    .map(|name| name.trim().to_owned())
                    .collect(),
            ),
        };
        for (name, section) in &self.config.providers {
            if name == "llamacpp" || only.as_ref().is_some_and(|only| !only.contains(name)) {
                continue;
            }
            // Resolved here: the keyring handle is not for other threads.
            let key = crate::secrets::resolve_api_key(
                name,
                section.api_key_env.as_deref(),
                &crate::secrets::OsKeyring,
            );
            // A key-optional server (vLLM) is asked with or without a key, but is
            // usually just not running, so nothing listening is no line on stderr.
            let optional = crate::provider::key_optional(name, Some(section));
            if key.is_none() && name != "ollama" && !optional {
                continue;
            }
            let (name, section) = (name.clone(), section.clone());
            let catalogs = Arc::clone(&self.catalogs);
            let open = Arc::clone(&self.open);
            let notifier = self.notifier.clone();
            std::thread::spawn(move || {
                if optional {
                    let base_url = section.base_url.clone().unwrap_or_else(|| {
                        crate::provider::server_kind(&name, Some(&section))
                            .map(|kind| kind.default_base_url().to_owned())
                            .unwrap_or_default()
                    });
                    if !crate::provider::catalog::server_listening(
                        &base_url,
                        std::time::Duration::from_millis(500),
                    ) {
                        return;
                    }
                }
                match crate::provider::catalog::fetch_catalog(&name, &section, key.as_deref()) {
                    Ok(entries) => {
                        let entries =
                            crate::provider::catalog::prefix_catalog_refs(&name, &entries);
                        catalogs
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(name, entries);
                        let sessions = open.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        if let Some(notify) = notifier {
                            for id in sessions {
                                notify(&id);
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("acp: model list for '{name}' unavailable: {}", error.0)
                    }
                }
            });
        }
    }

    /// The selectors an editor shows: the approval mode, and the models the config
    /// names (the default and the two routes, which is all the config lists) with
    /// the one in use, which `/model` may have set to something else.
    fn options_for(&self, session: &Session) -> Vec<ConfigOption> {
        let mut models: Vec<String> = Vec::new();
        for name in [
            self.config.model.default.as_str(),
            self.config.model.route.cheap.as_str(),
            self.config.model.route.frontier.as_str(),
            session.model_ref.as_str(),
        ] {
            if !name.is_empty() && !models.iter().any(|known| known == name) {
                models.push(name.to_owned());
            }
        }
        let mode = session.harness.gate.mode();
        // The configured models first, then each provider's own list, minus what is
        // already above (a value must be listed once).
        let labelled: Vec<(String, String)> = models
            .iter()
            .map(|model| (model.clone(), model.clone()))
            .collect();
        let mut groups = vec![("Configured".to_owned(), labelled.clone())];
        let mut choices = labelled;
        for (provider, entries) in self
            .catalogs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let fresh: Vec<(String, String)> = entries
                .iter()
                .filter(|(value, _)| !choices.iter().any(|(known, _)| known == value))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                choices.extend(fresh.iter().cloned());
                groups.push((provider.clone(), fresh));
            }
        }
        // One group would only add a heading to the short list.
        if groups.len() == 1 {
            groups.clear();
        }
        vec![
            ConfigOption {
                id: "mode".into(),
                name: "Approval mode".into(),
                description: Some(
                    "ask: confirm each change; auto: run without asking; deny: refuse changes"
                        .into(),
                ),
                category: "mode",
                current: mode.as_str().to_owned(),
                choices: ["ask", "auto", "deny"]
                    .iter()
                    .map(|value| ((*value).to_owned(), (*value).to_owned()))
                    .collect(),
                groups: Vec::new(),
            },
            ConfigOption {
                id: "model".into(),
                name: "Model".into(),
                description: Some(
                    "provider:model; /model switches to any other, for example one not listed"
                        .into(),
                ),
                category: "model",
                current: session.model_ref.clone(),
                choices,
                groups,
            },
        ]
    }
}

impl Backend for HarnessBackend {
    fn list_sessions(&mut self, cwd: Option<&Path>) -> Vec<SessionSummary> {
        let Some(store) = self.store_for_listing() else {
            return Vec::new();
        };
        let known = self
            .session_dirs()
            .map(|dirs| dirs.read())
            .unwrap_or_default();
        let wanted = cwd.map(|cwd| same_path_key(&crate::tools::display_path(cwd)));
        store
            .recent_sessions(200)
            .unwrap_or_default()
            .into_iter()
            // A session with no title has no user message yet: nothing to come back to.
            .filter(|info| info.title.is_some())
            .filter_map(|info| {
                let recorded = known.get(&info.session_id).and_then(Value::as_str);
                match (&wanted, recorded) {
                    // Filtered: only sessions known to work there. One whose
                    // directory was never recorded is left out, not guessed at.
                    (Some(wanted), Some(dir)) if *wanted != same_path_key(dir) => return None,
                    (Some(_), None) => return None,
                    _ => {}
                }
                Some(SessionSummary {
                    // The schema requires a directory, so an unfiltered list shows
                    // the one agent86 was started in, marked as a stand-in.
                    cwd: recorded.unwrap_or(&self.startup_cwd).to_owned(),
                    cwd_known: recorded.is_some(),
                    session_id: info.session_id,
                    title: info.title,
                    updated_at: info.updated_at,
                })
            })
            .take(100)
            .collect()
    }

    fn set_notifier(&mut self, notify: Notifier) {
        self.notifier = Some(notify);
    }

    fn config_options(&self, session_id: &str) -> Vec<ConfigOption> {
        self.sessions
            .get(session_id)
            .map(|session| self.options_for(session))
            .unwrap_or_default()
    }

    fn set_config_option(
        &mut self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<ConfigOption>, BackendError> {
        let mut session = self.sessions.remove(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        let outcome = match config_id {
            "mode" => match ApprovalMode::parse(value) {
                Some(mode) => {
                    session.harness.gate.set_mode(mode);
                    Ok(())
                }
                None => Err(format!("'{value}' is not one of ask, auto, deny")),
            },
            "model" => {
                // Only the listed choices: the picker offers them, and a free-form
                // ref is what `/model` is for.
                let listed = self.options_for(&session).iter().any(|option| {
                    option.id == "model" && option.choices.iter().any(|(v, _)| v == value)
                });
                if !listed {
                    Err(format!("'{value}' is not one of the listed models"))
                } else if value == session.model_ref {
                    Ok(())
                } else {
                    let said = self.switch_model(&mut session, value);
                    // A refusal leaves the model as it was, and says why.
                    if session.model_ref == value {
                        Ok(())
                    } else {
                        Err(said)
                    }
                }
            }
            other => Err(format!("Unknown config option: {other}")),
        };
        let options = self.options_for(&session);
        self.sessions.insert(session_id.to_owned(), session);
        outcome
            .map(|()| options)
            .map_err(BackendError::invalid_params)
    }

    fn commands(&self, session_id: &str) -> Vec<CommandInfo> {
        let mut commands: Vec<CommandInfo> = COMMANDS
            .iter()
            .chain([&SKILL_COMMAND, &AGENT_COMMAND])
            .map(|(name, description, hint)| CommandInfo {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                hint: hint.map(str::to_owned),
            })
            .collect();
        // Beyond Python: each skill that can be a command, as the terminal offers
        // them. Names are plain ASCII by then; descriptions come from files, so
        // they are flattened and stripped of control characters.
        if let Some(session) = self.sessions.get(session_id) {
            let skills = &session.harness.context.skills;
            commands.extend(
                crate::repl::skill_shortcuts(skills)
                    .into_iter()
                    .map(|skill| CommandInfo {
                        name: skill.name.clone(),
                        description: crate::skills::one_line(&skill.description)
                            .chars()
                            .filter(|c| !c.is_control())
                            .collect(),
                        hint: Some("task (optional)".to_owned()),
                    }),
            );
        }
        commands
    }

    fn take_title_change(&mut self, session_id: &str) -> Option<String> {
        self.sessions.get_mut(session_id)?.title_change.take()
    }

    fn run_command(&mut self, session_id: &str, line: &str) -> Option<String> {
        let (name, arg) = match line.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (line, ""),
        };
        let name = name.strip_prefix('/')?;
        if name == SKILL_COMMAND.0 {
            // A skill to run goes on to `prompt`; only a refusal is answered here.
            let session = self.sessions.get(session_id)?;
            return crate::repl::skill_refusal(&session.harness.context.skills, arg);
        }
        if name == AGENT_COMMAND.0 {
            // So does a specialty to run.
            let session = self.sessions.get(session_id)?;
            return crate::repl::parse_agent_command(&session.harness.context.specialties, arg)
                .err();
        }
        if !COMMANDS.iter().any(|(known, _, _)| *known == name) {
            return None;
        }
        // Take the session out so `self` can be borrowed beside it, and put it back.
        let mut session = self.sessions.remove(session_id)?;
        let output = match name {
            "mode" => {
                let gate = &session.harness.gate;
                if arg.is_empty() {
                    format!("approval mode: {}", gate.mode().as_str())
                } else {
                    match ApprovalMode::parse(arg) {
                        Some(mode) => {
                            gate.set_mode(mode);
                            format!("approval mode: {}", gate.mode().as_str())
                        }
                        None => format!("unknown mode '{arg}' (ask|auto|deny)"),
                    }
                }
            }
            "model" => self.switch_model(&mut session, arg),
            "thinking" => show_level("thinking", &mut session.thinking, arg),
            "toolview" => show_level("toolview", &mut session.tool_view, arg),
            "resume" => self.resume(&mut session, arg),
            "secret" => secret_command(&session, arg, self.client.secrets),
            "rename" => {
                // Answered here, so nothing reaches the model or the conversation.
                let (message, title) =
                    crate::repl::rename_session(session.store.as_ref(), &session.state, arg);
                session.title_change = title;
                message
            }
            "cost" => self.cost(&session),
            "skills" => {
                let skills = &session.harness.context.skills;
                if skills.is_empty() {
                    "no skills discovered".to_owned()
                } else {
                    crate::repl::skills_list(skills).plain_text()
                }
            }
            "agents" => crate::repl::agents_render(&session.harness, arg),
            _ => {
                let mut lines = vec![format!(
                    "tools: {}",
                    session.harness.registry.names().join(", ")
                )];
                if let Some(mcp) = &session.harness.mcp {
                    lines.extend(
                        mcp.server_statuses()
                            .into_iter()
                            .map(|(name, status)| format!("mcp {name}: {status}")),
                    );
                }
                lines.join("\n")
            }
        };
        self.sessions.insert(session_id.to_owned(), session);
        Some(output)
    }

    fn client_capabilities(&mut self, capabilities: ClientCapabilities) {
        self.client = capabilities;
    }

    fn new_session(
        &mut self,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<String, BackendError> {
        self.open(cwd, mcp_servers, None)
    }

    fn load_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<McpSpec>,
    ) -> Result<Vec<Update>, BackendError> {
        // Already open in this process: its harness stays, and its history replays.
        if !self.sessions.contains_key(session_id) {
            self.open(cwd, mcp_servers, Some(session_id))?;
        }
        let session = &self.sessions[session_id];
        Ok(replay(
            &session.state,
            &session.harness.context.secrets,
            &session.cards,
        ))
    }

    /// The memory methods on the session's own store, every text through its
    /// vault (beyond Python). A session without memory (`[memory] enabled =
    /// false`, or a database that would not open) refuses them.
    fn memory(
        &mut self,
        session_id: &str,
        request: MemoryRequest,
    ) -> Result<MemoryReply, BackendError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| BackendError::not_found(format!("Session not found: {session_id}")))?;
        let Some(store) = &session.store else {
            return Err(BackendError {
                code: super::INVALID_REQUEST,
                message: "Memory is off for this session".into(),
            });
        };
        let secrets = &session.harness.context.secrets;
        let failed = |error: crate::session::StoreError| BackendError::internal(error.to_string());
        let missing = |kind: EntryKind, id: i64| {
            BackendError::not_found(format!("No {} with id {id}", kind.as_str()))
        };
        match request {
            MemoryRequest::List(query) => Ok(MemoryReply::Entries(
                store
                    .list_entries(query)
                    .map_err(failed)?
                    .into_iter()
                    .map(|entry| redact_entry(entry, secrets))
                    .collect(),
            )),
            MemoryRequest::Pin { kind, id, pinned } => store
                .set_pinned(kind, id, pinned)
                .map_err(failed)?
                .map(|entry| MemoryReply::Entry(redact_entry(entry, secrets)))
                .ok_or_else(|| missing(kind, id)),
            MemoryRequest::Forget { kind, id } => {
                if store.forget(kind, id).map_err(failed)? {
                    Ok(MemoryReply::Forgotten)
                } else {
                    Err(missing(kind, id))
                }
            }
        }
    }

    /// The secret methods on the session's vault and the project keyring it was
    /// given (beyond Python): what the terminal's `/secret NAME`, `/secret`, and
    /// `/secret forget NAME` do.
    fn secrets(
        &mut self,
        session_id: &str,
        request: SecretMethod,
    ) -> Result<SecretReply, BackendError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| BackendError::not_found(format!("Session not found: {session_id}")))?;
        let vault = &session.harness.context.secrets;
        match request {
            SecretMethod::Set { name, answer } => {
                let wanted = answer.remember;
                let mut stored = vault.accept(&name, answer).map_err(|why| {
                    BackendError::invalid_params(format!("Invalid params: {}", why.describe(&name)))
                })?;
                if wanted && !vault.can_remember() && stored.warning.is_none() {
                    stored.warning = Some(
                        "no keyring is available to remember it, so it is set for this \
                         session only"
                            .to_owned(),
                    );
                }
                Ok(SecretReply::Set(stored))
            }
            SecretMethod::List => {
                let remembered = vault.remembered_names();
                Ok(SecretReply::List {
                    secrets: vault
                        .names()
                        .into_iter()
                        .map(|name| {
                            let kept = remembered.contains(&name);
                            (name, kept)
                        })
                        .collect(),
                    can_remember: vault.can_remember(),
                })
            }
            SecretMethod::Forget { name } => Ok(match vault.forget(&name) {
                Ok(forgotten) => SecretReply::Forgotten {
                    forgotten,
                    warning: None,
                },
                Err(error) => SecretReply::Forgotten {
                    forgotten: true,
                    warning: Some(format!(
                        "dropped from this session, but the keyring refused: {error}"
                    )),
                },
            }),
        }
    }

    fn cancel_handle(&self, session_id: &str) -> Option<CancelHandle> {
        let token = self.sessions.get(session_id)?.harness.cancel.clone();
        Some(Arc::new(move || token.cancel()))
    }

    fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        sink: Arc<dyn TurnSink>,
    ) -> Result<Stop, BackendError> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
            BackendError::invalid_params(format!("Unknown session: {session_id}"))
        })?;
        {
            let mut cards = session.cards.lock().unwrap_or_else(PoisonError::into_inner);
            cards.tools = Some(session.tool_view);
            cards.logs.clear();
        }
        let mut presenter = Presenter::new(
            session.thinking,
            session.tool_view,
            Arc::clone(&session.cards),
        )
        .with_policy(session.harness.context.policy.clone())
        .with_compaction(self.client.compaction);
        let prices = crate::pricing::PriceTable::from_config(&self.config);
        let default_ref = session.model_ref.clone();
        let Session {
            harness,
            store,
            state,
            sink: slot,
            ..
        } = session;
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&sink));
        let harness: &Harness = harness;
        let secrets = harness.context.secrets.clone();
        // Sent after each model call and once more when the turn ends; an update the
        // same as the last one sent is left out.
        let last_usage = std::cell::RefCell::new(None);
        let send_usage = |used: u64, total: &crate::types::Usage| {
            let update = usage_update(harness, &prices, &default_ref, used, total);
            let mut last = last_usage.borrow_mut();
            if last.as_ref() != Some(&update) {
                *last = Some(update.clone());
                sink.update(update);
            }
        };

        let on_event = |event: crate::agent::TurnEvent<'_>| match event {
            crate::agent::TurnEvent::Usage { step, session } => {
                send_usage(step.input_tokens, session);
            }
            event => presenter.event(event, sink.as_ref(), &secrets),
        };
        // Beyond Python: `/skill <name> [task]` and a skill's own `/<name> [task]` run
        // a turn with that skill chosen, as at a terminal.
        // Beyond Python: `/agent [--with-context] <name> <task>` runs a specialty.
        let agent = text
            .strip_prefix("/agent")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .and_then(|rest| {
                crate::repl::parse_agent_command(&harness.context.specialties, rest).ok()
            });
        let result =
            match (
                agent,
                crate::repl::skill_invocation(&harness.context.skills, text),
            ) {
                (Some((name, task, with_context)), _) => self.runtime.block_on(
                    harness.run_agent_turn(state, &name, &task, text, with_context, on_event),
                ),
                (None, Some((skill, task))) => self
                    .runtime
                    .block_on(harness.run_skill_turn(state, &skill, task, text, on_event)),
                (None, None) => self
                    .runtime
                    .block_on(harness.run_turn_events(state, text, text, on_event)),
            };
        presenter.end_thought(sink.as_ref());
        // The session's settled totals, sub-agents' spend and `/agent` turns included.
        if let Some(last) = state.steps.last() {
            send_usage(last.usage.input_tokens, &state.usage);
        }
        // The turn is over: nothing may be asked of the editor until the next one.
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        // Saved whether the turn finished or failed, so the session stays resumable.
        persist(store, state);
        match result {
            Ok(_) if harness.cancel.is_cancelled() => Ok(Stop::Cancelled),
            Ok(_) => Ok(Stop::EndTurn),
            Err(HarnessError::CircuitTripped(reason)) if reason.starts_with("step budget") => {
                Ok(Stop::MaxTurnRequests)
            }
            Err(error) => Err(BackendError::internal(error.to_string())),
        }
    }
}

/// Start the servers the client named and register their tools. A session with no
/// configured servers has no manager yet, so one is made. A server the client names
/// replaces a configured one of the same name for this session only: each session
/// has its own manager.
fn start_client_servers(config: &Config, harness: &mut Harness, servers: &[McpSpec]) {
    if servers.is_empty() {
        return;
    }
    let manager = match &harness.mcp {
        Some(manager) => Arc::clone(manager),
        None => {
            let manager = Arc::new(McpManager::empty(config));
            manager.attach(&harness.registry);
            harness.mcp = Some(Arc::clone(&manager));
            manager
        }
    };
    for spec in servers {
        let (server, literal) = server_config(spec);
        let name = spec.name();
        match manager.start_server(
            name,
            &server,
            &literal,
            &NoSecretStore,
            server.connect_timeout_s,
        ) {
            Ok(_) => {
                let (_, collisions) = manager.mount(name);
                if !collisions.is_empty() {
                    eprintln!(
                        "acp: MCP server '{name}': tools already taken by another server \
                         were skipped: {}",
                        collisions.join(", ")
                    );
                }
            }
            Err(reason) => {
                eprintln!("acp: MCP server '{name}' from the client failed to start: {reason}");
            }
        }
    }
}

/// The config for a server the client named, and the overrides that keep every
/// `${VAR}` in its values literal. The config layer expands `${VAR}` from the
/// environment and the keyring, which is right for a file the user wrote and wrong
/// for a value that arrives over the protocol: it must not be able to ask for the
/// agent's own keys. An override wins over both, so each reference maps to itself.
fn server_config(spec: &McpSpec) -> (McpServerConfig, HashMap<String, String>) {
    let mut texts: Vec<&str> = Vec::new();
    let value = match spec {
        McpSpec::Stdio {
            command, args, env, ..
        } => {
            texts.extend(args.iter().map(String::as_str));
            texts.extend(env.iter().map(|(_, value)| value.as_str()));
            serde_json::json!({
                "command": command,
                "args": args,
                "env": pairs_object(env),
                "transport": "stdio",
            })
        }
        McpSpec::Remote {
            url, sse, headers, ..
        } => {
            texts.push(url);
            texts.extend(headers.iter().map(|(_, value)| value.as_str()));
            serde_json::json!({
                "url": url,
                "headers": pairs_object(headers),
                "transport": if *sse { "sse" } else { "http" },
            })
        }
    };
    let literal = texts
        .iter()
        .flat_map(|text| var_ref_names(text))
        .map(|name| (name.clone(), format!("${{{name}}}")))
        .collect();
    let config = serde_json::from_value(value).expect("a server config always deserializes");
    (config, literal)
}

/// `[(name, value)]` as a JSON object, in order.
fn pairs_object(pairs: &[(String, String)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// A session's saved messages as the updates that tell them again: the user's
/// words, the agent's replies, each call it made, and what came back. The system
/// message is rebuilt per request and not part of the history.
fn replay(state: &AgentState, secrets: &crate::vault::SecretVault, cards: &Cards) -> Vec<Update> {
    let mut cards = cards.lock().unwrap_or_else(PoisonError::into_inner);
    let mut updates = Vec::new();
    for message in &state.messages {
        match message.role {
            Role::User if !message.content.is_empty() => {
                updates.push(Update::User(message.content.clone()))
            }
            Role::Assistant => {
                if !message.content.is_empty() {
                    updates.push(Update::Message(message.content.clone()));
                }
                for call in &message.tool_calls {
                    let id = cards.assign(&call.id);
                    cards.announced.insert(id.clone());
                    updates.push(Update::ToolCall {
                        id,
                        title: title(call, secrets),
                        name: call.name.clone(),
                        kind: tool_kind(&call.name),
                        raw_input: redacted_input(call, secrets),
                        locations: Vec::new(),
                    });
                }
            }
            // The saved message holds what the model was shown, not the result's
            // metadata, so a replayed write has no diff.
            Role::Tool => {
                if let Some(id) = &message.tool_call_id {
                    updates.push(Update::ToolResult {
                        id: cards.lookup(id),
                        ok: true,
                        text: message.content.clone(),
                        diff: None,
                        locations: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
    updates
}

/// A fact or episode with every vault value in its text replaced by the
/// value's placeholder, as everything else bound for the editor is.
fn redact_entry(mut entry: Entry, secrets: &crate::vault::SecretVault) -> Entry {
    entry.text = secrets.redact(&entry.text);
    entry.outcome = entry.outcome.map(|outcome| secrets.redact(&outcome));
    entry.category = entry.category.map(|category| secrets.redact(&category));
    entry
}

fn persist(store: &Option<SessionStore>, state: &AgentState) {
    if let Some(Err(error)) = store.as_ref().map(|store| store.persist(state)) {
        eprintln!("memory: could not save the session: {error}");
    }
}

/// The category of a built-in tool; an MCP server's tools and the rest are `other`.
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "list_dir" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "search_files" | "find_files" => ToolKind::Search,
        "run_command" | "python_exec" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// `name(arguments)`, the arguments as the model sent them, capped, with vault
/// secrets and control characters dealt with: the name of a tool comes from the
/// model (an MCP server names its own), and the editor shows the title as text.
fn title(call: &ToolCall, secrets: &crate::vault::SecretVault) -> String {
    // Beyond Python: a delegation's card is named for the agent and its task.
    if call.name == crate::tools::delegate::NAME {
        let text = |key: &str| {
            call.arguments
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        };
        let who = text("agent").or(text("role")).unwrap_or("assistant");
        let task = text("task").unwrap_or_default();
        let first = task.lines().next().unwrap_or_default();
        let first = if first.chars().count() > TITLE_ARGS {
            format!("{} ...", first.chars().take(TITLE_ARGS).collect::<String>())
        } else {
            first.to_owned()
        };
        return show_controls(&secrets.redact(&format!("{who}: {first}")));
    }
    let arguments = python_json_dumps(&Value::Object(call.arguments.clone()), false);
    let arguments = secrets.redact(&arguments);
    let arguments = if arguments.chars().count() > TITLE_ARGS {
        format!(
            "{} ...",
            arguments.chars().take(TITLE_ARGS).collect::<String>()
        )
    } else {
        arguments
    };
    show_controls(&format!("{}({arguments})", call.name))
}

/// `title` with the asking sub-agent's name ahead of it, when one asks.
fn labeled(preview: &ApprovalPreview, title: String) -> String {
    match &preview.agent {
        Some(agent) => format!("[{}] {title}", show_controls(agent)),
        None => title,
    }
}

/// The id a question about `call` names: the call's reported id, announced first
/// when the editor has not been shown it (a sub-agent's call, or `/agent`'s
/// approval of a project specialty).
fn announce_for_question(
    cards: &Cards,
    sink: &dyn TurnSink,
    call: &ToolCall,
    title: &str,
    secrets: &crate::vault::SecretVault,
    locations: &[Location],
) -> String {
    let mut state = cards.lock().unwrap_or_else(PoisonError::into_inner);
    let id = state.lookup(&call.id);
    if state.announced.insert(id.clone()) {
        sink.update(Update::ToolCall {
            id: id.clone(),
            title: title.to_owned(),
            name: call.name.clone(),
            kind: tool_kind(&call.name),
            raw_input: redacted_input(call, secrets),
            locations: locations.to_vec(),
        });
    }
    id
}

/// The tools whose `path` argument names one file they read or write.
const FILE_TOOLS: [&str; 3] = ["read_file", "write_file", "edit_file"];

/// Where `call` reads or writes, before it runs (beyond Python): the absolute,
/// jail-resolved path for the file tools, redacted; nothing for the rest, or for
/// a path the jail refuses (the call will fail and say so).
fn call_locations(
    call: &ToolCall,
    policy: Option<&crate::tools::SandboxPolicy>,
    secrets: &crate::vault::SecretVault,
) -> Vec<Location> {
    let path = FILE_TOOLS
        .contains(&call.name.as_str())
        .then(|| call.arguments.get("path").and_then(Value::as_str))
        .flatten();
    match (policy, path) {
        (Some(policy), Some(path)) => match policy.resolve_within(path) {
            Ok(target) => vec![Location {
                path: secrets.redact(&crate::tools::display_path(&target)),
                line: None,
            }],
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Where a change lands: its file, at the first changed line.
fn change_location(change: &FileChange) -> Location {
    Location {
        path: change.path.clone(),
        line: change.line,
    }
}

/// The change a file tool recorded in its result, for a `diff` content item.
/// `dispatch` redacted it already, so a secret is its placeholder here.
fn result_diff(result: &ToolResult) -> Option<FileChange> {
    if !result.ok {
        return None;
    }
    FileChange::from_metadata(&result.metadata)
}

/// Where a file tool's write landed, from its result (recorded even when the texts
/// were over the cap and left out).
fn result_location(result: &ToolResult) -> Option<Location> {
    let path = result.metadata.get("abs_path")?.as_str()?;
    Some(Location {
        path: path.to_owned(),
        line: result
            .metadata
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|line| usize::try_from(line).ok()),
    })
}

/// The call's arguments for the editor, with every vault value replaced by its
/// `${secret:NAME}` placeholder.
fn redacted_input(call: &ToolCall, secrets: &crate::vault::SecretVault) -> Value {
    let mut input = Value::Object(call.arguments.clone());
    secrets.redact_value(&mut input);
    input
}

/// What the user decides on: the warning if there is one, then the tool's own
/// detail (a diff, a command) or the argument summary, controls shown not sent.
/// Already redacted by the preview.
fn approval_text(preview: &ApprovalPreview) -> String {
    let detail = preview.detail.as_deref().unwrap_or(&preview.summary);
    let text = match &preview.warning {
        Some(warning) => format!("Warning: {warning}\n\n{detail}"),
        None => detail.to_owned(),
    };
    show_controls(&text)
}

/// A `usage_update` (beyond Python): `used` is the last call's input tokens, as the
/// terminal's status line counts it, against the routed model's window, and the
/// session's cost when that model has a price (an unpriced model's cost is unknown,
/// so it is left out rather than sent as zero).
fn usage_update(
    harness: &Harness,
    prices: &crate::pricing::PriceTable,
    default_ref: &str,
    used: u64,
    session: &crate::types::Usage,
) -> Update {
    let reference = harness
        .routed_ref()
        .unwrap_or_else(|| default_ref.to_owned());
    Update::Usage {
        used,
        size: harness.active_context_window(),
        cost: prices.is_priced(&reference).then_some(session.cost_usd),
    }
}

/// A result for the editor: what the model is shown, or the error when that is
/// empty.
fn result_text(result: &ToolResult, observation: &str) -> String {
    if !observation.trim().is_empty() {
        return observation.to_owned();
    }
    match result.error.as_deref().filter(|error| !error.is_empty()) {
        Some(error) => error.to_owned(),
        None => "(no output)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_entries_reach_the_editor_with_vault_values_as_placeholders() {
        let secrets = crate::vault::SecretVault::default();
        secrets.set("DEPLOY_TOKEN", "tok-0123456789".into());
        let entry = Entry {
            kind: EntryKind::Episode,
            id: 4,
            ts: 1_700_000_000.0,
            text: "deploy with tok-0123456789".into(),
            outcome: Some("used tok-0123456789".into()),
            session_id: Some("sess-1".into()),
            category: None,
            pinned_at: None,
        };
        let shown = redact_entry(entry, &secrets);
        assert_eq!(shown.text, "deploy with ${secret:DEPLOY_TOKEN}");
        assert_eq!(
            shown.outcome.as_deref(),
            Some("used ${secret:DEPLOY_TOKEN}")
        );
        assert_eq!(shown.session_id.as_deref(), Some("sess-1"));
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    /// Records what a turn tells the editor.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<Update>>);

    impl TurnSink for Recorder {
        fn update(&self, update: Update) {
            self.0.lock().unwrap().push(update);
        }
        fn ask(&self, _: PermissionRequest) -> Permission {
            Permission::Declined
        }
        fn call(&self, _: &str, _: Value, _: Option<Duration>) -> Result<Value, String> {
            Err("no editor".into())
        }
    }

    fn outcome(ok: bool, content: &str) -> ToolResult {
        ToolResult {
            call_id: "c1".into(),
            name: "list_dir".into(),
            ok,
            content: content.into(),
            error: (!ok).then(|| "it broke".to_owned()),
            metadata: Default::default(),
        }
    }

    #[test]
    fn repeated_provider_ids_are_reported_under_ids_of_their_own() {
        let mut cards = CardState::default();
        assert_eq!(cards.assign("ollama-0"), "ollama-0");
        assert_eq!(cards.assign("ollama-0"), "ollama-0#1");
        assert_eq!(cards.lookup("ollama-0"), "ollama-0#1");
        assert_eq!(cards.assign("ollama-0#1x"), "ollama-0#1x");
        assert_eq!(cards.assign(""), "#2");
        // Replay draws from the same set, so a loaded session's next call is new.
        let mut state = AgentState::new();
        let call = call("list_dir", json!({}));
        state.messages.push(crate::types::Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![call.clone(), call.clone()],
            tool_call_id: None,
            name: None,
        });
        let cards: Cards = Arc::default();
        let updates = replay(&state, &crate::vault::SecretVault::default(), &cards);
        let ids: Vec<&str> = updates
            .iter()
            .filter_map(|update| match update {
                Update::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["c1", "c1#1"]);
        assert_eq!(cards.lock().unwrap().assign("c1"), "c1#2");
    }

    #[test]
    fn a_delegation_is_titled_for_its_agent_and_task() {
        let secrets = crate::vault::SecretVault::default();
        let delegate = |arguments: Value| title(&call("delegate", arguments), &secrets);
        assert_eq!(
            delegate(json!({"agent": "reviewer", "task": "Check auth\nthen tests"})),
            "reviewer: Check auth"
        );
        assert_eq!(delegate(json!({"role": "coder", "task": "x"})), "coder: x");
        assert_eq!(
            delegate(json!({"task": "x\u{1b}[2J"})),
            "assistant: x\u{241b}[2J"
        );
    }

    #[test]
    fn a_sub_agent_fills_its_card_at_the_sessions_level() {
        use crate::subagent::{Card, SubagentEvent};
        let secrets = crate::vault::SecretVault::default();
        let run = |level: DisplayLevel, card: &Card| {
            let cards: Cards = Arc::default();
            {
                let mut state = cards.lock().unwrap();
                state.tools = Some(level);
                if card.announced {
                    let id = state.assign(&card.id);
                    state.announced.insert(id);
                }
            }
            let sink = Recorder::default();
            let inner = call("list_dir", json!({"path": "."}));
            let ok = outcome(true, "a.txt");
            let failed = outcome(false, "");
            for event in [
                SubagentEvent::Started {
                    card,
                    run: 1,
                    name: "planner",
                    depth: 1,
                    task: "Plan it\nfully",
                },
                SubagentEvent::ToolCall { card, call: &inner },
                SubagentEvent::ToolResult {
                    card,
                    call: &inner,
                    result: &ok,
                },
                SubagentEvent::ToolResult {
                    card,
                    call: &inner,
                    result: &failed,
                },
                SubagentEvent::Finished {
                    card,
                    run: 1,
                    name: "planner",
                    depth: 1,
                    ok: true,
                    usage: &crate::types::Usage::default(),
                },
            ] {
                on_subagent(&cards, &sink, &secrets, &event);
            }
            sink.0.into_inner().unwrap()
        };
        let delegation = Card::new("d", true);
        let full = run(DisplayLevel::Full, &delegation);
        let Update::ToolProgress { id, text } = full.last().unwrap() else {
            panic!("{full:?}");
        };
        assert_eq!(id, "d");
        assert_eq!(
            text,
            "planner started\nlist_dir({\"path\": \".\"})\n  \u{21b3} a.txt\n\u{2717} list_dir: error: it broke"
        );
        // The delegation's card is ended by its own result, not here.
        assert!(!full
            .iter()
            .any(|update| matches!(update, Update::ToolResult { .. })));
        let summary = run(DisplayLevel::Summary, &delegation);
        let Update::ToolProgress { text, .. } = summary.last().unwrap() else {
            panic!("{summary:?}");
        };
        assert_eq!(text, "1 tool call\n\u{2717} list_dir: error: it broke");
        assert!(run(DisplayLevel::Hidden, &delegation).is_empty());

        // `/agent` has no call of its own: its card is announced and ended here.
        let synthetic = run(DisplayLevel::Full, &Card::new("agent-planner-1", false));
        assert!(matches!(
            &synthetic[0],
            Update::ToolCall { id, title, .. } if id == "agent-planner-1" && title == "planner: Plan it"
        ));
        assert!(matches!(
            synthetic.last().unwrap(),
            Update::ToolResult { id, ok: true, .. } if id == "agent-planner-1"
        ));
    }

    /// Run a turn of events through a presenter at these levels.
    fn presented(thinking: DisplayLevel, tools: DisplayLevel, ok: bool) -> Vec<Update> {
        let sink = Recorder::default();
        let secrets = crate::vault::SecretVault::default();
        let mut presenter = Presenter::new(thinking, tools, Cards::default());
        let call = call("list_dir", json!({"path": "."}));
        let result = outcome(ok, "a.txt\nb.txt");
        for event in [
            TurnEvent::Thinking("let me "),
            TurnEvent::Thinking("look"),
            TurnEvent::ToolCall {
                call: &call,
                text: "[tool] list_dir(...)",
            },
            TurnEvent::ToolResult {
                call: &call,
                result: &result,
                observation: "a.txt\nb.txt",
                text: "[tool] list_dir -> ok",
            },
            TurnEvent::Text("Done."),
        ] {
            presenter.event(event, &sink, &secrets);
        }
        presenter.end_thought(&sink);
        sink.0.into_inner().unwrap()
    }

    fn kinds(updates: &[Update]) -> Vec<&'static str> {
        updates
            .iter()
            .map(|update| match update {
                Update::Thought(_) => "thought",
                Update::ToolCall { .. } => "call",
                Update::ToolResult { .. } => "result",
                Update::Message(_) => "message",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn full_shows_everything_as_it_arrives() {
        let updates = presented(DisplayLevel::Full, DisplayLevel::Full, true);
        assert_eq!(
            kinds(&updates),
            ["thought", "thought", "call", "result", "message"]
        );
        let Update::ToolCall { raw_input, .. } = &updates[2] else {
            panic!()
        };
        assert_eq!(raw_input, &json!({"path": "."}));
        let Update::ToolResult { text, .. } = &updates[3] else {
            panic!()
        };
        assert_eq!(text, "a.txt\nb.txt");
    }

    #[test]
    fn summary_folds_reasoning_to_one_line_and_a_call_to_its_name_and_a_result_line() {
        let updates = presented(DisplayLevel::Summary, DisplayLevel::Summary, true);
        // The reasoning ended at the tool call, so its line comes before it.
        assert_eq!(kinds(&updates), ["thought", "call", "result", "message"]);
        let Update::Thought(line) = &updates[0] else {
            panic!()
        };
        assert!(line.starts_with("∴ thought for "), "{line}");
        assert!(line.ends_with("11 chars"), "{line}");
        let Update::ToolCall {
            raw_input, title, ..
        } = &updates[1]
        else {
            panic!()
        };
        assert_eq!(raw_input, &Value::Null, "no arguments in the summary");
        assert!(title.starts_with("list_dir("));
        let Update::ToolResult { text, .. } = &updates[2] else {
            panic!()
        };
        assert_ne!(text, "a.txt\nb.txt");
        assert!(!text.contains('\n'), "{text}");
    }

    #[test]
    fn hidden_shows_only_the_answer_and_a_failed_call() {
        let updates = presented(DisplayLevel::Hidden, DisplayLevel::Hidden, true);
        assert_eq!(kinds(&updates), ["message"]);

        // A failure is the user's to see, so the call comes with it.
        let updates = presented(DisplayLevel::Hidden, DisplayLevel::Hidden, false);
        assert_eq!(kinds(&updates), ["call", "result", "message"]);
        let Update::ToolResult { ok, text, .. } = &updates[1] else {
            panic!()
        };
        assert!(!ok);
        assert!(
            text.contains("a.txt") || text.contains("it broke"),
            "{text}"
        );
    }

    #[test]
    fn levels_are_set_shown_and_checked() {
        let mut level = DisplayLevel::Full;
        assert_eq!(show_level("thinking", &mut level, ""), "thinking: full");
        assert_eq!(
            show_level("thinking", &mut level, "hidden"),
            "thinking: hidden (from the next turn)"
        );
        assert_eq!(level, DisplayLevel::Hidden);
        assert_eq!(
            show_level("thinking", &mut level, "loud"),
            "unknown level 'loud' (full|summary|hidden)"
        );
        assert_eq!(level, DisplayLevel::Hidden);
    }

    #[test]
    fn built_in_tools_have_kinds_and_the_rest_are_other() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("list_dir"), ToolKind::Read);
        assert_eq!(tool_kind("write_file"), ToolKind::Edit);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("search_files"), ToolKind::Search);
        assert_eq!(tool_kind("find_files"), ToolKind::Search);
        assert_eq!(tool_kind("run_command"), ToolKind::Execute);
        assert_eq!(tool_kind("python_exec"), ToolKind::Execute);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("firecrawl_scrape"), ToolKind::Other);
    }

    #[test]
    fn a_title_names_the_tool_and_caps_and_cleans_its_arguments() {
        let secrets = crate::vault::SecretVault::default();
        let short = title(&call("read_file", json!({"path": "a.rs"})), &secrets);
        assert_eq!(short, r#"read_file({"path": "a.rs"})"#);

        let long = title(
            &call("write_file", json!({"content": "x".repeat(500)})),
            &secrets,
        );
        assert!(long.ends_with(" ...)"), "{long}");
        assert!(long.chars().count() < 140, "{}", long.chars().count());

        // A tool name from an MCP server can carry control characters.
        let hostile = title(&call("evil\u{1b}[2J", json!({})), &secrets);
        assert!(!hostile.contains('\u{1b}'), "{hostile:?}");
    }

    #[test]
    fn what_the_user_approves_is_the_detail_with_the_warning_first() {
        let preview = ApprovalPreview {
            summary: "{\"path\": \"a\"}".into(),
            detail: Some("--- a/a\n+++ b/a\r\n@@".into()),
            lexer: Some("diff"),
            warning: None,
            full: false,
            agent: None,
            change: None,
            asker: None,
        };
        let text = approval_text(&preview);
        assert!(text.starts_with("--- a/a"));
        assert!(
            !text.contains('\r'),
            "controls are shown, not sent: {text:?}"
        );

        let warned = ApprovalPreview {
            warning: Some("changes what agent86 runs".into()),
            full: false,
            agent: None,
            detail: None,
            ..preview
        };
        let text = approval_text(&warned);
        assert!(text.starts_with("Warning: changes what agent86 runs\n\n"));
        assert!(
            text.ends_with("{\"path\": \"a\"}"),
            "falls back to the summary"
        );
    }

    #[test]
    fn a_result_shows_the_observation_or_else_the_error() {
        let failed = ToolResult {
            call_id: "c1".into(),
            name: "x".into(),
            ok: false,
            content: String::new(),
            error: Some("denied".into()),
            metadata: Default::default(),
        };
        assert_eq!(result_text(&failed, "saw this"), "saw this");
        assert_eq!(result_text(&failed, "  "), "denied");
        let empty = ToolResult {
            error: None,
            ..failed
        };
        assert_eq!(result_text(&empty, ""), "(no output)");
    }

    fn default_config() -> Config {
        let dir = std::env::temp_dir().join(format!("agent86-acp-state-{}", std::process::id()));
        let paths = crate::config::ConfigPaths {
            user: None,
            project: dir.join("absent.toml"),
            project_trust: crate::config::ProjectTrust::Trusted,
        };
        crate::config::load_config(&paths, &crate::config::Overrides::default()).unwrap()
    }

    fn record(kind: &str, fields: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("ts".into(), json!(1.5));
        record.insert("session".into(), json!("s1"));
        record.insert("kind".into(), json!(kind));
        if let Value::Object(fields) = fields {
            record.extend(fields);
        }
        record
    }

    fn states(sink: &Recorder) -> Vec<Value> {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|update| match update {
                Update::State(event) => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn state_events_forward_only_their_kinds_and_spend_resets_each_turn() {
        let reporter = StateReporter::new(
            &default_config(),
            Vec::new(),
            Cards::default(),
            crate::vault::SecretVault::default(),
        );
        let sink = Recorder::default();
        let call = |cost: f64| {
            record(
                "model_call",
                json!({"step": 1, "model": "gpt-4o", "cost_usd": cost}),
            )
        };
        for event in [
            record("turn_start", json!({"task": "go"})),
            call(0.5),
            record(
                "guardrail",
                json!({"stage": "ingress_input", "findings": "injection"}),
            ),
            record("route", json!({"model": "cheap"})),
            record("tool_call", json!({"tool": "list_dir"})),
            call(0.25),
            record("turn_end", json!({"status": "ok"})),
            record("turn_start", json!({"task": "again"})),
            call(0.125),
        ] {
            reporter.record(&sink, &event);
        }
        let events = states(&sink);
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "model_call",
                "guardrail",
                "route",
                "model_call",
                "model_call"
            ]
        );
        // The recorder's session label is dropped; the notification names it.
        assert!(events.iter().all(|event| event.get("session").is_none()));
        assert_eq!(events[1]["findings"], "injection");
        assert_eq!(events[2]["model"], "cheap");
        assert_eq!(events[0]["turn_cost_usd"], 0.5);
        assert_eq!(events[3]["turn_cost_usd"], 0.75);
        // A new turn starts its spend again.
        assert_eq!(events[4]["turn_cost_usd"], 0.125);
        assert_eq!(events[0]["cost_cap_usd"], 5.0);
        assert_eq!(events[0]["priced"], true);
    }

    #[test]
    fn a_call_is_priced_by_its_cost_its_model_or_a_configured_ref() {
        let reporter = StateReporter::new(
            &default_config(),
            vec!["ollama:llama3.1".into(), String::new()],
            Cards::default(),
            crate::vault::SecretVault::default(),
        );
        // Built in under its bare id, even when it cost nothing.
        assert!(reporter.priced("gpt-4o", 0.0));
        // A configured local model is free, not unknown.
        assert!(reporter.priced("llama3.1", 0.0));
        // Unknown and free: no price.
        assert!(!reporter.priced("mystery-model", 0.0));
        // A cost means a price was found.
        assert!(reporter.priced("mystery-model", 0.01));
    }

    #[test]
    fn a_sub_agents_task_reaches_the_client_as_the_trace_would_hold_it() {
        let secrets = crate::vault::SecretVault::default();
        secrets.set("API_TOKEN", "tok_9f8e7d6c5b4a3921".to_owned());
        let cards = Cards::default();
        let reporter =
            StateReporter::new(&default_config(), Vec::new(), Arc::clone(&cards), secrets);
        let sink = Recorder::default();
        let card = crate::subagent::Card::new("call_9", true);
        let task = format!("Use tok_9f8e7d6c5b4a3921 to {}", "x".repeat(400));
        reporter.subagent(
            &sink,
            &SubagentEvent::Started {
                card: &card,
                run: 3,
                name: "planner",
                depth: 1,
                task: &task,
            },
        );
        reporter.subagent(
            &sink,
            &SubagentEvent::Finished {
                card: &card,
                run: 3,
                name: "planner",
                depth: 1,
                ok: false,
                usage: &crate::types::Usage {
                    input_tokens: 7,
                    output_tokens: 2,
                    cost_usd: 0.01,
                    ..Default::default()
                },
            },
        );
        let events = states(&sink);
        assert_eq!(events.len(), 2);
        let started = &events[0];
        assert_eq!(started["kind"], "subagent");
        assert_eq!(started["phase"], "started");
        assert_eq!(started["card"], "call_9");
        let shown = started["task"].as_str().unwrap();
        assert!(shown.starts_with("Use ${secret:API_TOKEN} to "), "{shown}");
        assert!(!shown.contains("tok_9f8e7d6c5b4a3921"));
        // Cut as the trace's `spawn` cuts it.
        assert!(shown.chars().count() <= 200 + "${secret:API_TOKEN}".len());
        let finished = &events[1];
        assert_eq!(finished["phase"], "finished");
        assert_eq!(finished["ok"], false);
        assert_eq!(finished["input_tokens"], 7);
        assert_eq!(finished["cost_usd"], 0.01);
    }
}
