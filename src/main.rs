use agent86::agent::HarnessError;
use agent86::approval::terminal_prompt_if_interactive;
use agent86::config::{load_config, Config, ConfigPaths, Overrides};
use agent86::keys;
use agent86::mcp::McpManager;
use agent86::observability::{
    otlp_document, parse_since, python_repr_value, read_trace, trace_generations, TraceFilter,
    SPAN_KINDS,
};
use agent86::pricing::PriceTable;
use agent86::pyjson::{python_json_dumps, python_json_dumps_indented};
use agent86::secrets::{OsKeyring, SecretStore, SERVICE_NAME};
use agent86::session::{expand_home, AgentState, SessionStore};
use agent86::setup::{assemble, SetupError, MODEL_HELP};
use agent86::turn::{format_turn_summary, RunOutput};
use agent86::types::ApprovalMode;
use agent86::ui::run_interactive;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "agent86",
    bin_name = "agent86",
    version,
    about = "An agentic harness on the command line - connect to remote or local models \
             and let them use tools and skills.",
    disable_help_subcommand = true
)]
struct Cli {
    /// Override model (provider:model).
    #[arg(short, long)]
    model: Option<String>,

    /// HITL mode: auto|ask|deny.
    #[arg(long)]
    approval: Option<String>,

    /// Resume a prior session by id (REPL).
    #[arg(short, long)]
    resume: Option<String>,

    /// Force the plain REPL (no status line / spinner / hotkeys).
    #[arg(long)]
    plain: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single goal non-interactively (scriptable).
    Run {
        /// The goal for the agent to accomplish.
        goal: String,

        /// Emit structured JSON output.
        #[arg(long = "json")]
        json: bool,

        /// Auto-approve side-effecting tools (non-interactive).
        #[arg(short, long)]
        yes: bool,

        /// Resume/continue a session by id.
        #[arg(short, long)]
        session: Option<String>,
    },

    /// Inspect the flight-data recorder.
    #[command(subcommand)]
    Trace(TraceCommand),

    /// Store or delete provider API keys in the OS keyring.
    #[command(subcommand)]
    Keys(KeysCommand),

    /// Manage MCP servers.
    #[command(subcommand)]
    Mcp(McpCommand),
}

#[derive(Subcommand)]
enum McpCommand {
    /// List configured MCP servers.
    List,

    /// Start configured MCP servers and list the tools they expose.
    Tools,
}

#[derive(Subcommand)]
enum TraceCommand {
    /// Show the trace file location.
    Path,

    /// Show recent events from the flight recorder.
    Show {
        /// Filter to one session.
        #[arg(short, long)]
        session: Option<String>,

        /// Max events to show.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,

        /// Only this event kind (repeatable), e.g. -k tool_call.
        #[arg(short, long)]
        kind: Vec<String>,

        /// Only events newer than this duration, e.g. 30m, 2h, 7d.
        #[arg(long)]
        since: Option<String>,
    },

    /// Export the trace: raw events, or spans reconstructed into the OTLP JSON shape.
    Export {
        /// Export one session (default: every session in the trace).
        #[arg(short, long)]
        session: Option<String>,

        /// jsonl (filtered events) | json (one array) | otlp-json.
        #[arg(short = 'f', long = "format", default_value = "jsonl")]
        format: String,

        /// Write here instead of stdout.
        #[arg(short, long)]
        out: Option<String>,

        /// Max events to consider.
        #[arg(short = 'n', long, default_value_t = 100_000)]
        limit: usize,

        /// Only events newer than this duration, e.g. 2h.
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Store a provider's API key, prompting for it with input hidden.
    Set {
        /// The provider section name, e.g. openrouter.
        provider: String,

        /// Read the key from stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
    },

    /// Delete a provider's stored API key.
    Delete {
        /// The provider section name, e.g. openrouter.
        provider: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stderr = Stderr::new();

    let config = match load_config(
        &ConfigPaths::discover(),
        &Overrides::from_env(cli.model, cli.approval),
    ) {
        Ok(config) => config,
        Err(error) => {
            stderr.error(&error.to_string());
            stderr.dim("Fix the TOML, or move that file aside to fall back to the defaults.");
            return ExitCode::from(1);
        }
    };

    match cli.command {
        Some(Command::Run {
            goal,
            json,
            yes,
            session,
        }) => {
            let mut config = config;
            if yes {
                config.guardrails.approval = ApprovalMode::Auto;
            }
            run(&config, &goal, json, session.as_deref(), &stderr)
        }
        Some(Command::Keys(command)) => keys_command(&config, command, &stderr),
        Some(Command::Trace(command)) => trace_command(&config, command, &stderr),
        Some(Command::Mcp(command)) => mcp_command(&config, command, &stderr),
        None => {
            run_interactive(
                config,
                cli.resume.as_deref(),
                cli.plain,
                Box::new(OsKeyring),
            );
            ExitCode::SUCCESS
        }
    }
}

fn run(
    config: &Config,
    goal: &str,
    as_json: bool,
    session: Option<&str>,
    stderr: &Stderr,
) -> ExitCode {
    // At a terminal, `ask` prompts for each side-effecting call; piped or in CI
    // there is no prompt, so `ask` declines them unless --yes was given.
    let assembly = match assemble(config, terminal_prompt_if_interactive()) {
        Ok(assembly) => assembly,
        Err(SetupError::Model(message)) => {
            stderr.error(&message);
            stderr.dim(MODEL_HELP);
            return ExitCode::from(1);
        }
        Err(SetupError::Workspace(message)) => {
            stderr.error(&message);
            return ExitCode::from(1);
        }
    };
    let harness = assembly.harness;
    let store = assembly.store;
    let priced = PriceTable::from_config(config).is_priced(&assembly.model_ref.to_string());

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            stderr.error(&format!("could not start the async runtime: {error}"));
            return ExitCode::from(1);
        }
    };

    if !as_json {
        if let Some(note) = &assembly.memory_note {
            stderr.dim(&format!("memory: {note}"));
        }
        if let Some(note) = &assembly.sandbox_note {
            stderr.warn(&format!("sandbox: {note}"));
        }
    }

    // Resume the named session, or start a new one (also when the id is unknown).
    let resumed = match (&store, session) {
        (Some(store), Some(id)) => match store.load(id) {
            Ok(found) => found,
            Err(error) => {
                stderr.error(&error.to_string());
                return ExitCode::from(1);
            }
        },
        _ => None,
    };
    let mut state = match resumed {
        Some(state) => state,
        None => {
            let state = AgentState::new();
            persist(store.as_ref(), &state, as_json, stderr);
            state
        }
    };

    let mut stdout = std::io::stdout().lock();
    let result = runtime.block_on(harness.run_turn(&mut state, goal, |text| {
        if !as_json {
            // Raw write + flush streams partial lines reliably across terminals.
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
    }));
    // Saved whether the turn finished or failed, so the session stays resumable.
    persist(store.as_ref(), &state, as_json, stderr);

    let streamed = match result {
        Ok(streamed) => streamed,
        Err(error) => {
            stderr.error_after_stream(&error.to_string());
            // Python adds the model hint for provider failures only.
            if matches!(error, HarnessError::Provider(_)) {
                stderr.dim(MODEL_HELP);
            }
            return ExitCode::from(1);
        }
    };
    let output = RunOutput {
        session_id: state.session_id.clone(),
        // Honor the egress guardrail (e.g. redact mode) on the machine-readable output.
        output: harness.guardrails.inspect_egress(&streamed).1,
        steps: state.step_count() as u64,
        usage: state.usage,
        turn: state.last_turn.clone(),
    };

    if as_json {
        match serde_json::to_string_pretty(&output) {
            Ok(document) => {
                let _ = writeln!(stdout, "{document}");
            }
            Err(error) => {
                stderr.error(&format!("could not encode the JSON output: {error}"));
                return ExitCode::from(1);
            }
        }
    } else {
        let _ = writeln!(stdout);
        let _ = stdout.flush();
        if let Some(turn) = &output.turn {
            // stderr: stdout is the answer, and `agent86 run ... > out.txt` must keep only that.
            stderr.dim(&format_turn_summary(turn, priced));
        }
    }
    ExitCode::SUCCESS
}

fn mcp_command(config: &Config, command: McpCommand, stderr: &Stderr) -> ExitCode {
    match command {
        McpCommand::List => {
            if config.mcp_servers.is_empty() {
                stdout_dim(
                    "No MCP servers configured. Add an `[mcp_servers.<name>]` block to your                      config file.",
                    stderr,
                );
                return ExitCode::SUCCESS;
            }
            let rows: Vec<Vec<String>> = config
                .mcp_servers
                .iter()
                .map(|(name, server)| {
                    let endpoint = server.url.clone().unwrap_or_else(|| {
                        std::iter::once(server.command.clone().unwrap_or_default())
                            .chain(server.args.iter().cloned())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .trim()
                            .to_owned()
                    });
                    vec![
                        name.clone(),
                        server.transport().to_owned(),
                        if server.enabled { "yes" } else { "no" }.to_owned(),
                        endpoint,
                    ]
                })
                .collect();
            print_table(&["Name", "Transport", "Enabled", "Endpoint"], &rows);
        }
        McpCommand::Tools => {
            let Some(manager) = McpManager::start(config) else {
                stdout_dim(
                    "No MCP servers configured (or MCP disabled). `agent86 mcp list` shows what                      is configured and whether each is enabled.",
                    stderr,
                );
                return ExitCode::SUCCESS;
            };
            // One line per failed server, so none is skimmed past.
            for note in &manager.notes() {
                stderr.warn(note);
            }
            let rows: Vec<Vec<String>> = manager
                .tools()
                .iter()
                .map(|tool| vec![tool.name().to_owned(), tool.description().to_owned()])
                .collect();
            if rows.is_empty() {
                stdout_dim("No MCP tools discovered.", stderr);
            } else {
                print_table(&["Tool", "Description"], &rows);
            }
        }
    }
    ExitCode::SUCCESS
}

/// A plain left-aligned table; Python renders the same columns as a Rich table.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let render = |cells: Vec<&str>| {
        let last = cells.len() - 1;
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                if index == last {
                    (*cell).to_owned()
                } else {
                    format!("{cell:<width$}", width = widths[index])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", render(headers.to_vec()));
    for row in rows {
        println!("{}", render(row.iter().map(String::as_str).collect()));
    }
}

fn trace_command(config: &Config, command: TraceCommand, stderr: &Stderr) -> ExitCode {
    let observability = &config.observability;
    let path = expand_home(&observability.path).join("trace.jsonl");
    let keep = observability.keep_traces;
    let since = |since: &Option<String>| -> Result<Option<f64>, String> {
        match since {
            None => Ok(None),
            Some(text) => {
                let seconds = parse_since(text)
                    .map_err(|message| format!("Invalid value for '--since': {message}"))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |elapsed| elapsed.as_secs_f64());
                Ok(Some(now - seconds))
            }
        }
    };
    let usage_error = |message: &str| {
        stderr.error(message);
        ExitCode::from(2)
    };

    match command {
        TraceCommand::Path => {
            let exists = if path.exists() { "exists" } else { "not found" };
            println!("{} ({exists})", path.display());
            for older in trace_generations(&path, keep)
                .into_iter()
                .filter(|older| *older != path)
            {
                let size = std::fs::metadata(&older).map_or(0, |metadata| metadata.len());
                stdout_dim(
                    &format!("{} ({} bytes)", older.display(), thousands(size)),
                    stderr,
                );
            }
            ExitCode::SUCCESS
        }
        TraceCommand::Show {
            session,
            limit,
            kind,
            since: since_text,
        } => {
            let after_ts = match since(&since_text) {
                Ok(after) => after,
                Err(message) => return usage_error(&message),
            };
            let filter = TraceFilter {
                session,
                kinds: (!kind.is_empty()).then_some(kind),
                after_ts,
            };
            let events = read_trace(&path, &filter, limit, keep);
            if events.is_empty() {
                stdout_dim("no trace events", stderr);
                return ExitCode::SUCCESS;
            }
            print_trace_table(&events, stderr);
            ExitCode::SUCCESS
        }
        TraceCommand::Export {
            session,
            format,
            out,
            limit,
            since: since_text,
        } => {
            let choice = format.to_lowercase().replace('_', "-");
            if !matches!(choice.as_str(), "jsonl" | "json" | "otlp-json") {
                return usage_error(&format!(
                    "Invalid value: unknown format {} (jsonl | json | otlp-json)",
                    agent86::types::python_repr(&format)
                ));
            }
            let after_ts = match since(&since_text) {
                Ok(after) => after,
                Err(message) => return usage_error(&message),
            };
            let filter = TraceFilter {
                session,
                kinds: (choice == "otlp-json")
                    .then(|| SPAN_KINDS.iter().map(|kind| (*kind).to_owned()).collect()),
                after_ts,
            };
            let events = read_trace(&path, &filter, limit, keep);
            let values: Vec<serde_json::Value> = events
                .iter()
                .cloned()
                .map(serde_json::Value::Object)
                .collect();
            let text = match choice.as_str() {
                "jsonl" => values
                    .iter()
                    .map(|event| format!("{}\n", python_json_dumps(event, false)))
                    .collect::<String>(),
                "json" => format!(
                    "{}\n",
                    python_json_dumps_indented(&serde_json::Value::Array(values), false, 2)
                ),
                _ => format!(
                    "{}\n",
                    python_json_dumps_indented(&otlp_document(&events), false, 2)
                ),
            };
            match out {
                None => {
                    let mut stdout = std::io::stdout().lock();
                    let _ = stdout.write_all(text.as_bytes());
                    let _ = stdout.flush();
                }
                Some(out) => {
                    let target = expand_home(&out);
                    if let Some(parent) = target
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                    {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // As Python's write_text: text mode, so `\n` is `\r\n` on Windows.
                    let on_disk = if cfg!(windows) {
                        text.replace('\n', "\r\n")
                    } else {
                        text
                    };
                    if let Err(error) = std::fs::write(&target, on_disk) {
                        stderr.error(&format!("could not write {}: {error}", target.display()));
                        return ExitCode::from(1);
                    }
                    println!(
                        "Wrote {} event(s) to {} ({choice}).",
                        events.len(),
                        target.display()
                    );
                }
            }
            ExitCode::SUCCESS
        }
    }
}

/// A dim line on stdout, as Python's `console.print("[dim]...")`.
fn stdout_dim(message: &str, stderr: &Stderr) {
    if stderr.color && std::io::stdout().is_terminal() {
        println!("\x1b[2m{message}\x1b[0m");
    } else {
        println!("{message}");
    }
}

/// `1234567` as `1,234,567`, as Python's `{:,}`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The trace as a plain aligned table: time, session, kind, tokens, cost, detail.
/// Python renders the same columns as a Rich table.
fn print_trace_table(events: &[serde_json::Map<String, serde_json::Value>], stderr: &Stderr) {
    const SKIP: &[&str] = &[
        "ts",
        "session",
        "kind",
        "input_tokens",
        "output_tokens",
        "cost_usd",
    ];
    let mut rows: Vec<[String; 7]> = Vec::new();
    let (mut spent, mut tokens_in, mut tokens_out) = (0.0_f64, 0_u64, 0_u64);
    for event in events {
        let text = |name: &str| match event.get(name) {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(other) => python_repr_value(other),
        };
        let is_call = event.get("kind").and_then(serde_json::Value::as_str) == Some("model_call");
        let cost = event.get("cost_usd").and_then(serde_json::Value::as_f64);
        if is_call {
            spent += cost.unwrap_or(0.0);
            tokens_in += event
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            tokens_out += event
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
        }
        let detail: serde_json::Map<String, serde_json::Value> = event
            .iter()
            .filter(|(key, _)| !SKIP.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let clock = event
            .get("ts")
            .and_then(serde_json::Value::as_f64)
            .and_then(|ts| chrono::DateTime::from_timestamp(ts.trunc() as i64, 0))
            .map(|utc| {
                utc.with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        rows.push([
            clock,
            text("session").chars().take(12).collect(),
            text("kind"),
            if is_call {
                text("input_tokens")
            } else {
                String::new()
            },
            if is_call {
                text("output_tokens")
            } else {
                String::new()
            },
            match (is_call, cost) {
                (true, Some(cost)) => format!("${cost:.4}"),
                _ => String::new(),
            },
            python_repr_value(&serde_json::Value::Object(detail))
                .chars()
                .take(100)
                .collect(),
        ]);
    }

    let headers = ["time", "session", "kind", "in", "out", "cost", "detail"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let right_aligned = [false, false, false, true, true, true, false];
    let render = |cells: [&str; 7]| {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let pad = widths[index].saturating_sub(cell.chars().count());
                if index == 6 {
                    (*cell).to_owned()
                } else if right_aligned[index] {
                    format!("{}{cell}", " ".repeat(pad))
                } else {
                    format!("{cell}{}", " ".repeat(pad))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", render(headers));
    for row in &rows {
        println!("{}", render(row.each_ref().map(String::as_str)));
    }
    if tokens_in > 0 || tokens_out > 0 || spent > 0.0 {
        stdout_dim(
            &format!(
                "{} in / {} out tokens, ${spent:.4} across {} event(s)",
                thousands(tokens_in),
                thousands(tokens_out),
                events.len()
            ),
            stderr,
        );
    }
}

/// Save the session if there is a store; a failure is reported, not fatal.
fn persist(store: Option<&SessionStore>, state: &AgentState, as_json: bool, stderr: &Stderr) {
    if let Some(Err(error)) = store.map(|store| store.persist(state)) {
        if !as_json {
            stderr.dim(&format!("memory: could not save the session: {error}"));
        }
    }
}

fn keys_command(config: &Config, command: KeysCommand, stderr: &Stderr) -> ExitCode {
    let store = OsKeyring;
    let (provider, result) = match command {
        KeysCommand::Set { provider, stdin } => {
            let result = keys::check_provider(config, &provider)
                .and_then(|()| keys::read_key(&provider, stdin))
                .and_then(|key| {
                    store
                        .set_password(SERVICE_NAME, &provider, &key)
                        .map_err(|error| {
                            format!("Could not store the key for '{provider}': {error}")
                        })
                })
                .map(|()| {
                    format!(
                        "Stored the key for '{provider}' in the OS keyring \
                         (service \"{SERVICE_NAME}\", account \"{provider}\")."
                    )
                });
            (provider, result)
        }
        KeysCommand::Delete { provider } => {
            let result = keys::check_provider(config, &provider).and_then(|()| {
                match store.delete_password(SERVICE_NAME, &provider) {
                    Ok(true) => Ok(format!("Deleted the stored key for '{provider}'.")),
                    Ok(false) => Ok(format!("No key was stored for '{provider}'.")),
                    Err(error) => Err(format!(
                        "Could not delete the key for '{provider}': {error}"
                    )),
                }
            });
            (provider, result)
        }
    };

    match result {
        Ok(message) => {
            println!("{message}");
            if let Some(variable) = keys::overriding_variable(config, &provider) {
                stderr.dim(&format!(
                    "Note: {variable} is set in this environment and takes precedence over the keyring."
                ));
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            stderr.error(&message);
            ExitCode::from(1)
        }
    }
}

/// stderr writer that styles like Rich: colour only on a terminal, and never with `NO_COLOR`.
struct Stderr {
    color: bool,
}

impl Stderr {
    fn new() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self {
            color: std::io::stderr().is_terminal() && !no_color,
        }
    }

    fn error(&self, message: &str) {
        self.write("", "\x1b[31m", "error:", message);
    }

    /// An error printed after streamed text, starting on a fresh line.
    fn error_after_stream(&self, message: &str) {
        self.write("\n", "\x1b[31m", "error:", message);
    }

    /// A yellow notice, as Rich's `[yellow]...[/yellow]`.
    fn warn(&self, message: &str) {
        if self.color {
            eprintln!("\x1b[33m{message}\x1b[0m");
        } else {
            eprintln!("{message}");
        }
    }

    fn dim(&self, message: &str) {
        if self.color {
            eprintln!("\x1b[2m{message}\x1b[0m");
        } else {
            eprintln!("{message}");
        }
    }

    fn write(&self, lead: &str, style: &str, label: &str, message: &str) {
        if self.color {
            eprintln!("{lead}{style}{label}\x1b[0m {message}");
        } else {
            eprintln!("{lead}{label} {message}");
        }
    }
}
