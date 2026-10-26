use agent86::{
    config::{self, Overrides},
    orchestration::{format_turn_summary, run_turn},
    provider::{provider_for_ref, ModelProvider},
    types::ModelRef,
};
use clap::{CommandFactory, Parser, Subcommand};
use std::{
    io::{self, Write},
    process::ExitCode,
};

/// What to do next when a model cannot be built or reached.
const MODEL_HELP: &str = "Pick a model with `--model provider:model`. The Rust port supports \
     OpenAI-compatible providers: openai, openrouter, groq, or any [providers.<name>] block \
     with a base_url.";

const CONFIG_HELP: &str = "Fix the TOML, or move that file aside to fall back to the defaults.";

/// An agentic harness on the command line (experimental Rust implementation).
#[derive(Parser)]
#[command(name = "agent86", version, about)]
struct Cli {
    /// Override model (provider:model).
    #[arg(short, long, global = true)]
    model: Option<String>,

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
        as_json: bool,
        /// Auto-approve side-effecting tools (non-interactive). Accepted for
        /// compatibility; this port has no tools yet.
        #[arg(short, long)]
        yes: bool,
        /// Resume/continue a session by id. Not supported yet.
        #[arg(short, long, value_name = "SESSION")]
        session: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            // The interactive REPL is not ported yet.
            let _ = Cli::command().print_help();
            ExitCode::SUCCESS
        }
        Some(Command::Run {
            goal,
            as_json,
            yes: _,
            session,
        }) => {
            if session.is_some() {
                eprintln!(
                    "error: --session is not supported yet; the Rust port does not persist \
                     sessions."
                );
                return ExitCode::from(2);
            }
            run(cli.model, &goal, as_json)
        }
    }
}

fn run(model_flag: Option<String>, goal: &str, as_json: bool) -> ExitCode {
    let config = match config::load(&Overrides::from_env(model_flag)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("{CONFIG_HELP}");
            return ExitCode::from(1);
        }
    };

    let provider = match ModelRef::parse(&config.default_model)
        .map_err(|err| err.to_string())
        .and_then(|reference| provider_for_ref(&reference, &config).map_err(|err| err.to_string()))
    {
        Ok(provider) => provider,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("{MODEL_HELP}");
            return ExitCode::from(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: could not start the async runtime: {err}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(run_one(provider.as_ref(), goal, as_json))
}

async fn run_one(provider: &dyn ModelProvider, goal: &str, as_json: bool) -> ExitCode {
    let mut stdout = io::stdout();
    let mut emit = |text: &str| {
        if !as_json {
            // Raw write + flush keeps partial-line streaming visible on every terminal.
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
    };

    let output = match run_turn(provider, goal, &mut emit).await {
        Ok(output) => output,
        Err(err) => {
            eprintln!("\nerror: {err}");
            eprintln!("{MODEL_HELP}");
            return ExitCode::from(1);
        }
    };

    let mut stdout = io::stdout().lock();
    if as_json {
        let json = match serde_json::to_string_pretty(&output) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("error: could not encode the JSON result: {err}");
                return ExitCode::from(1);
            }
        };
        if writeln!(stdout, "{json}").is_err() {
            return ExitCode::from(1);
        }
    } else {
        let _ = writeln!(stdout);
        let _ = stdout.flush();
        if let Some(turn) = &output.turn {
            // stderr: stdout is the answer, and `agent86 run ... > out.txt` must keep
            // getting only that.
            eprintln!("{}", format_turn_summary(turn));
        }
    }
    ExitCode::SUCCESS
}
