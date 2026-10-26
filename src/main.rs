use agent86::agent::{Harness, HarnessError};
use agent86::approval::{terminal_prompt_if_interactive, ApprovalGate};
use agent86::config::{load_config, Config, ConfigPaths, Overrides};
use agent86::guardrails::Guardrails;
use agent86::keys;
use agent86::pricing::PriceTable;
use agent86::prompt::build_system_prompt;
use agent86::provider::provider_for_ref;
use agent86::secrets::{OsKeyring, SecretStore, SERVICE_NAME};
use agent86::session::{expand_home, AgentState, SessionStore};
use agent86::tools::sandbox::build_executor;
use agent86::tools::web::WebSettings;
use agent86::tools::{builtin_registry, ToolContext};
use agent86::turn::{format_turn_summary, RunOutput};
use agent86::types::ApprovalMode;
use agent86::types::ModelRef;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

/// What to do next when a model cannot be built: a bad ref, an unknown provider, a missing key.
///
/// Python points at `agent86 models`, which is not ported yet.
const MODEL_HELP: &str = "Pick an OpenAI-compatible provider with `--model provider:model` \
                          (e.g. `openrouter:openai/gpt-4o-mini`) and set its API key environment variable \
                          or store the key in the OS keyring.";

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

    /// Store or delete provider API keys in the OS keyring.
    #[command(subcommand)]
    Keys(KeysCommand),
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
        None => {
            eprintln!(
                "agent86: the interactive REPL is not implemented yet in the Rust port; \
                 use `agent86 run <goal>`"
            );
            ExitCode::from(2)
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
    let provider = ModelRef::parse(&config.model.default)
        .map_err(|error| error.to_string())
        .and_then(|reference| {
            provider_for_ref(&reference, config)
                .map(|provider| (provider, reference))
                .map_err(|error| error.to_string())
        });
    let (provider, reference) = match provider {
        Ok(built) => built,
        Err(message) => {
            stderr.error(&message);
            stderr.dim(MODEL_HELP);
            return ExitCode::from(1);
        }
    };

    let max_tokens = config.max_output_tokens_for(&reference.provider);
    let priced = PriceTable::from_config(config).is_priced(&reference.to_string());

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

    // Tools are jailed to the directory `agent86` runs in, as in Python.
    let mut context = match std::env::current_dir().and_then(|dir| ToolContext::new(&dir)) {
        Ok(context) => context,
        Err(error) => {
            stderr.error(&format!("could not open the workspace: {error}"));
            return ExitCode::from(1);
        }
    };
    context.policy.timeout = Duration::from_secs(config.limits.tool_timeout_s);
    context.policy.env_passthrough = config.sandbox.env_passthrough.clone();
    context.web = WebSettings {
        allow_private: config.tools.web_allow_private,
        user_agent: config.tools.web_user_agent.clone(),
        max_chars: config.tools.web_max_chars,
    };
    let (executor, sandbox_note) = build_executor(&config.sandbox);
    context.executor = executor;

    // Sessions live in the memory database; if it cannot be opened, run statelessly.
    let store = if config.memory.enabled {
        match SessionStore::open(&expand_home(&config.memory.path)) {
            Ok(store) => Some(store),
            Err(error) => {
                if !as_json {
                    stderr.dim(&format!(
                        "memory: unavailable, running without recall or session history. {error}"
                    ));
                }
                None
            }
        }
    } else {
        None
    };
    if let Some(note) = sandbox_note.filter(|_| !as_json) {
        stderr.warn(&format!("sandbox: {note}"));
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
    let harness = Harness {
        provider: provider.as_ref(),
        registry: builtin_registry(),
        context,
        limits: config.limits.clone(),
        system: build_system_prompt(config),
        max_tokens,
        // At a terminal, `ask` prompts for each side-effecting call; piped or in CI
        // there is no prompt, so `ask` declines them unless --yes was given.
        gate: ApprovalGate::new(config.guardrails.approval, terminal_prompt_if_interactive()),
        guardrails: Guardrails::new(&config.guardrails),
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
