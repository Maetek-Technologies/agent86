//! `agent86-eval`: the eval set for agent86's harness (slice 0 of the
//! local-model work in `docs/PROJECT_PLAN.md`; see `docs/evals.md`).
//!
//! Each task is a small repository with a goal and a hidden check. A run copies
//! the task's workspace somewhere fresh, gives `agent86 run --json` the goal in
//! an isolated home, and then runs the check over what the agent left. Nothing
//! here is part of the default test suite's network use: only `run` talks to a
//! model, and only when asked.
//!
//! ```text
//! cargo build --release
//! cargo run -p agent86-eval -- list
//! cargo run -p agent86-eval -- validate
//! cargo run -p agent86-eval -- run --model vllm:cyankiwi/Qwen3.6-35B-A3B-AWQ-4bit --window 32768
//! ```

mod process;
mod report;
mod run;
mod task;

use clap::{Parser, Subcommand};
use run::{Record, Setup, Variant};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};

#[derive(Parser)]
#[command(name = "agent86-eval", about = "Run agent86's eval set")]
struct Cli {
    /// The evals directory (holding `tasks/`, `config.toml`, and `variants/`).
    #[arg(long, global = true)]
    evals: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the tasks.
    List,
    /// Check that every task's check fails untouched and passes when solved.
    Validate {
        /// Only these tasks.
        #[arg(long = "task")]
        tasks: Vec<String>,
    },
    /// Run tasks against models and score them.
    Run(Box<RunArgs>),
}

#[derive(clap::Args)]
struct RunArgs {
    /// A `provider:model` ref; repeat for several.
    #[arg(long = "model", required = true)]
    models: Vec<String>,
    /// Only these tasks (by id); repeat for several.
    #[arg(long = "task")]
    tasks: Vec<String>,
    /// Only tasks of these categories; repeat for several.
    #[arg(long = "category")]
    categories: Vec<String>,
    /// Runs of each task per model and variant.
    #[arg(long, default_value_t = 1)]
    repeat: u32,
    /// A config overlay to compare against the baseline (a TOML file, named
    /// after its stem); repeat for several.
    #[arg(long = "variant")]
    variants: Vec<PathBuf>,
    /// Leave the baseline out when variants are given.
    #[arg(long)]
    no_baseline: bool,
    /// Give every model this context window (`[model.context_window]`).
    #[arg(long)]
    window: Option<u64>,
    /// Runs at a time.
    #[arg(long, default_value_t = 1)]
    jobs: usize,
    /// The agent86 binary (default: target/release, else target/debug).
    #[arg(long)]
    agent86: Option<PathBuf>,
    /// Where results go (default: target/evals/<time>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Do not take `[providers]` and `[pricing]` from your own config.
    #[arg(long)]
    no_user_providers: bool,
    /// Keep each run's workspace and home (under the system temp directory).
    #[arg(long)]
    keep: bool,
    /// Continue an earlier eval in this results directory: runs that finished
    /// with their provider reachable are kept, and only the missing or
    /// unreachable ones run. Give the same models, tasks, variants, and repeats.
    #[arg(long, conflicts_with = "out")]
    resume: Option<PathBuf>,
    /// Stop the eval after this many runs in a row could not reach their
    /// provider (a server down or rebooting), rather than fail every run left;
    /// 0 never stops. `--resume` picks up where it stopped.
    #[arg(long, default_value_t = 3)]
    stop_after_unreachable: usize,
}

/// A run's place in the grid: task, model, variant, repeat.
type RunKey = (String, String, String, u32);

fn key_of(record: &Record) -> RunKey {
    (
        record.task.clone(),
        record.model.clone(),
        record.variant.clone(),
        record.repeat,
    )
}

/// The runs an earlier eval in `dir` finished with its provider reachable, the
/// last of each kept, so `--resume` neither repeats nor loses them.
fn finished_runs(dir: &Path) -> Result<Vec<Record>, String> {
    let path = dir.join("results.jsonl");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut kept: Vec<Record> = Vec::new();
    for (number, line) in text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let record: Record = serde_json::from_str(line)
            .map_err(|error| format!("{} line {}: {error}", path.display(), number + 1))?;
        // Results written before `infra` existed carry only the error text.
        let unreachable = record.infra
            || record
                .error
                .as_deref()
                .is_some_and(run::provider_unreachable);
        if unreachable {
            continue;
        }
        kept.retain(|earlier| key_of(earlier) != key_of(&record));
        kept.push(record);
    }
    Ok(kept)
}

/// The repository root this binary was built from.
fn repo_root() -> PathBuf {
    // Not `canonicalize`: on Windows it gives a `\\?\` path, which Python and
    // `cmd.exe` cannot use once a `/` is joined to it.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The newer of `target/release/agent86` and `target/debug/agent86`, so a
/// release build left from weeks ago is never run in place of the current one.
fn default_agent86() -> Result<PathBuf, String> {
    let name = format!("agent86{}", std::env::consts::EXE_SUFFIX);
    let target = repo_root().join("target");
    ["release", "debug"]
        .iter()
        .map(|profile| target.join(profile).join(&name))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
        .ok_or_else(|| {
            "no agent86 binary in target/; run `cargo build --release` or pass --agent86".into()
        })
}

/// What `agent86 --version` says, to put the binary on record.
fn agent86_version(agent86: &Path) -> String {
    std::process::Command::new(agent86)
        .arg("--version")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown version".into())
}

fn user_providers() -> Result<String, String> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let Some(home) = std::env::var_os(variable).filter(|home| !home.is_empty()) else {
        return Ok(String::new());
    };
    match std::fs::read_to_string(PathBuf::from(home).join(".agent86").join("config.toml")) {
        Ok(text) => run::providers_of(&text),
        Err(_) => Ok(String::new()),
    }
}

fn main() {
    let cli = Cli::parse();
    let evals = cli.evals.unwrap_or_else(|| repo_root().join("evals"));
    let result = match cli.command {
        Command::List => list(&evals),
        Command::Validate { tasks } => validate(&evals, &tasks),
        Command::Run(args) => run_all(&evals, *args),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn list(evals: &Path) -> Result<(), String> {
    for task in task::load_tasks(&evals.join("tasks"))? {
        println!(
            "{:<28} {:<14} {}",
            task.id, task.file.category, task.file.measures
        );
    }
    Ok(())
}

fn validate(evals: &Path, ids: &[String]) -> Result<(), String> {
    let python = task::find_python().ok_or("no python3 or python on PATH")?;
    let tasks = task::select(task::load_tasks(&evals.join("tasks"))?, ids, &[])?;
    let scratch =
        std::env::temp_dir().join(format!("agent86-eval-validate-{}", std::process::id()));
    let mut failed = 0;
    for task in &tasks {
        match task::validate(task, &scratch, &python) {
            Ok(()) => println!("ok    {}", task.id),
            Err(error) => {
                failed += 1;
                println!("FAIL  {error}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if failed > 0 {
        return Err(format!(
            "{failed} of {} tasks failed validation",
            tasks.len()
        ));
    }
    println!("{} tasks valid", tasks.len());
    Ok(())
}

fn run_all(evals: &Path, args: RunArgs) -> Result<(), String> {
    let python = task::find_python().ok_or("no python3 or python on PATH")?;
    let tasks = task::select(
        task::load_tasks(&evals.join("tasks"))?,
        &args.tasks,
        &args.categories,
    )?;
    if tasks.is_empty() {
        return Err("no tasks selected".into());
    }
    let mut variants = Vec::new();
    if args.variants.is_empty() || !args.no_baseline {
        variants.push(Variant::baseline());
    }
    for path in &args.variants {
        variants.push(Variant::from_file(path)?);
    }
    let agent86 = match args.agent86 {
        Some(path) => path,
        None => default_agent86()?,
    };
    let version = agent86_version(&agent86);
    eprintln!("agent86: {} ({version})", agent86.display());
    let stamp = jiff::ZonedDateTime::now()
        .strftime("%Y%m%d-%H%M%S")
        .to_string();
    let (out_dir, finished) = match &args.resume {
        Some(dir) => (dir.clone(), finished_runs(dir)?),
        None => (
            args.out
                .clone()
                .unwrap_or_else(|| repo_root().join("target").join("evals").join(&stamp)),
            Vec::new(),
        ),
    };
    std::fs::create_dir_all(&out_dir).map_err(|error| format!("{}: {error}", out_dir.display()))?;
    let eval_config = std::fs::read_to_string(evals.join("config.toml")).unwrap_or_default();
    let setup = Setup {
        agent86,
        python,
        out_dir: out_dir.clone(),
        scratch: std::env::temp_dir().join("agent86-eval"),
        providers: if args.no_user_providers {
            String::new()
        } else {
            user_providers()?
        },
        eval_config,
        window: args.window,
        keep: args.keep,
    };

    let done: std::collections::HashSet<RunKey> = finished.iter().map(key_of).collect();
    let mut queue = VecDeque::new();
    for model in &args.models {
        for variant in &variants {
            for task in &tasks {
                for repeat in 1..=args.repeat {
                    let key = (task.id.clone(), model.clone(), variant.name.clone(), repeat);
                    if !done.contains(&key) {
                        queue.push_back((task.clone(), model.clone(), variant.clone(), repeat));
                    }
                }
            }
        }
    }
    let total = queue.len();
    eprintln!(
        "{total} runs: {} tasks x {} models x {} variants x {} repeats{} -> {}",
        tasks.len(),
        args.models.len(),
        variants.len(),
        args.repeat,
        if finished.is_empty() {
            String::new()
        } else {
            format!(", {} already finished", finished.len())
        },
        out_dir.display()
    );
    let queue = Mutex::new(queue);
    let (sender, receiver) = mpsc::channel::<Record>();
    let results_path = out_dir.join("results.jsonl");
    // On a resume, the file is rewritten with the runs kept, so it ends with
    // one line per run.
    let mut results = std::fs::File::create(&results_path)
        .map_err(|error| format!("{}: {error}", results_path.display()))?;
    for record in &finished {
        if let Ok(line) = serde_json::to_string(record) {
            let _ = writeln!(results, "{line}");
        }
    }
    let mut records = finished;
    let mut fresh = 0;
    let mut streak = 0;
    let mut stopped = false;
    std::thread::scope(|scope| {
        for _ in 0..args.jobs.max(1) {
            let sender = sender.clone();
            let (queue, setup) = (&queue, &setup);
            scope.spawn(move || loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some((task, model, variant, repeat)) = next else {
                    break;
                };
                let record = run::run_one(setup, &task, &model, &variant, repeat);
                if sender.send(record).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        for record in receiver {
            // Written as each run ends, so an interrupted eval keeps what it did.
            if let Ok(line) = serde_json::to_string(&record) {
                let _ = writeln!(results, "{line}");
            }
            fresh += 1;
            eprintln!(
                "[{fresh}/{total}] {} {} | {} | {} #{} ({} steps, {:.0}s){}",
                if record.infra {
                    "DOWN"
                } else if record.passed {
                    "pass"
                } else {
                    "FAIL"
                },
                record.task,
                record.model,
                record.variant,
                record.repeat,
                record.steps,
                record.duration_s,
                record.error.as_deref().map_or(String::new(), |e| {
                    format!(
                        " - {}",
                        e.lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(120)
                            .collect::<String>()
                    )
                })
            );
            // A provider that stays unreachable fails every run left; stop
            // handing out work instead, and let `--resume` finish later.
            streak = if record.infra { streak + 1 } else { 0 };
            if args.stop_after_unreachable > 0 && streak >= args.stop_after_unreachable && !stopped
            {
                stopped = true;
                queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
                eprintln!(
                    "stopping: {streak} runs in a row could not reach their provider; \
                     the runs already started will finish"
                );
            }
            records.push(record);
        }
    });
    let mut summary = format!(
        "{}\nBinary: `{}` ({version}). Runs: {}.\n",
        report::summary(&records),
        setup.agent86.display(),
        records.len()
    );
    if stopped {
        summary.push_str(&format!(
            "\nStopped early: the provider stopped answering. Continue with `--resume {}` and the same arguments.\n",
            out_dir.display()
        ));
    }
    let summary_path = out_dir.join("summary.md");
    std::fs::write(&summary_path, &summary)
        .map_err(|error| format!("{}: {error}", summary_path.display()))?;
    println!("{summary}");
    eprintln!("results: {}", out_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every task in the repository is honest: its check fails on the untouched
    /// workspace and passes with the reference solution. Needs Python, as the
    /// MCP tests do, and skips without it.
    #[test]
    fn every_task_validates() {
        let Some(python) = task::find_python() else {
            eprintln!("skipped: no python3 or python on PATH");
            return;
        };
        let tasks = task::load_tasks(&repo_root().join("evals").join("tasks")).unwrap();
        assert!(!tasks.is_empty());
        let scratch =
            std::env::temp_dir().join(format!("agent86-eval-test-{}", std::process::id()));
        let failures: Vec<String> = tasks
            .iter()
            .filter_map(|task| task::validate(task, &scratch, &python).err())
            .collect();
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn a_resume_keeps_finished_runs_and_drops_unreachable_ones() {
        let dir = std::env::temp_dir().join(format!("agent86-eval-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let row = |task: &str, repeat: u32, passed: bool, extra: &str| {
            format!(
                "{{\"task\":\"{task}\",\"category\":\"bugfix\",\"model\":\"m:x\",\"variant\":\"baseline\",\
                 \"repeat\":{repeat},\"passed\":{passed},\"agent_exit\":0,\"timed_out\":false,\"steps\":3,\
                 \"tool_calls\":1,\"input_tokens\":1,\"output_tokens\":1,\"cost_usd\":0.0,\"compactions\":0,\
                 \"context_trims\":0,\"duration_s\":1.0,\"check_tail\":\"\",\"final\":\"\"{extra}}}"
            )
        };
        let lines = [
            row("a", 1, true, ",\"error\":null"),
            row("b", 1, false, ",\"error\":\"x\",\"infra\":true"),
            // Written before `infra` existed: only the error text says so.
            row(
                "c",
                1,
                false,
                ",\"error\":\"agent86 wrote no result: Cannot reach http://box:8000\"",
            ),
            row("d", 1, false, ",\"error\":\"Circuit tripped\""),
            // A rerun of `a` replaces the earlier row.
            row("a", 1, false, ",\"error\":null"),
        ];
        std::fs::write(dir.join("results.jsonl"), lines.join("\n") + "\n").unwrap();
        let kept = finished_runs(&dir).unwrap();
        let summary: Vec<(String, bool)> =
            kept.iter().map(|r| (r.task.clone(), r.passed)).collect();
        assert_eq!(summary, [("d".to_owned(), false), ("a".to_owned(), false)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreachable_providers_are_told_from_model_failures() {
        assert!(run::provider_unreachable(
            "error: Cannot reach http://spark:8000/v1/chat/completions: error sending request"
        ));
        assert!(run::provider_unreachable(
            "error: vllm: the connection to http://spark:8000 failed while streaming model \"q\""
        ));
        assert!(run::provider_unreachable(
            "error: HTTP 503: upstream unavailable"
        ));
        assert!(!run::provider_unreachable(
            "error: Circuit tripped: aborted after 4 consecutive tool errors"
        ));
        assert!(!run::provider_unreachable("error: HTTP 400: bad request"));
    }

    #[test]
    fn every_variant_parses() {
        let dir = repo_root().join("evals").join("variants");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                Variant::from_file(&path).unwrap();
            }
        }
    }
}
