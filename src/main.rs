use agent86::config::{load_config, Config, ConfigPaths, Overrides};
use agent86::pricing::PriceTable;
use agent86::prompt::build_system_prompt;
use agent86::provider::provider_for_ref;
use agent86::turn::{format_turn_summary, run_text_turn};
use agent86::types::ModelRef;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

/// What to do next when a model cannot be built: a bad ref, an unknown provider, a missing key.
///
/// Python points at `agent86 models`, which is not ported yet.
const MODEL_HELP: &str = "Pick an OpenAI-compatible provider with `--model provider:model` \
                          (e.g. `openrouter:openai/gpt-4o-mini`) and set its API key environment variable.";

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
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stderr = Stderr::new();

    let config = match load_config(&ConfigPaths::discover(), &Overrides::from_env(cli.model)) {
        Ok(config) => config,
        Err(error) => {
            stderr.error(&error.to_string());
            stderr.dim("Fix the TOML, or move that file aside to fall back to the defaults.");
            return ExitCode::from(1);
        }
    };

    match cli.command {
        Some(Command::Run { goal, json }) => run(&config, &goal, json, &stderr),
        None => {
            eprintln!(
                "agent86: the interactive REPL is not implemented yet in the Rust port; \
                 use `agent86 run <goal>`"
            );
            ExitCode::from(2)
        }
    }
}

fn run(config: &Config, goal: &str, as_json: bool, stderr: &Stderr) -> ExitCode {
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

    let mut stdout = std::io::stdout().lock();
    let result = runtime.block_on(run_text_turn(
        provider.as_ref(),
        build_system_prompt(config),
        goal,
        max_tokens,
        |text| {
            if !as_json {
                // Raw write + flush streams partial lines reliably across terminals.
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
        },
    ));

    let output = match result {
        Ok(output) => output,
        Err(error) => {
            stderr.error_after_stream(&error.to_string());
            stderr.dim(MODEL_HELP);
            return ExitCode::from(1);
        }
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
