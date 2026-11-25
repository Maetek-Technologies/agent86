//! One run: a task, a model, and a variant, in a fresh workspace and a fresh
//! isolated home, scored by the task's hidden check.

use crate::process::{run, Outcome};
use crate::task::Task;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use toml_edit::{value, DocumentMut, Item, Table};

/// How much longer than its own wall-clock budget the agent may run before the
/// runner kills it.
const KILL_GRACE: Duration = Duration::from_secs(60);

/// How much of the final answer a record keeps.
const FINAL_CHARS: usize = 600;

/// A named configuration overlay; `baseline` has none.
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub overlay: String,
}

impl Variant {
    pub fn baseline() -> Self {
        Self {
            name: "baseline".into(),
            overlay: String::new(),
        }
    }

    /// A variant from a TOML file, named after its stem.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let overlay = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        overlay
            .parse::<DocumentMut>()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("{}: no file name", path.display()))?
            .to_owned();
        Ok(Self { name, overlay })
    }
}

/// What every run of one invocation shares.
#[derive(Debug, Clone)]
pub struct Setup {
    pub agent86: PathBuf,
    pub python: String,
    pub out_dir: PathBuf,
    pub scratch: PathBuf,
    /// The `[providers]` and `[pricing]` tables of the user's own config, as TOML.
    pub providers: String,
    /// `evals/config.toml`.
    pub eval_config: String,
    /// `--window`: the context window every model is given.
    pub window: Option<u64>,
    /// `--keep`: leave each run's workspace and home behind.
    pub keep: bool,
}

/// One row of `results.jsonl`.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub task: String,
    pub category: String,
    pub model: String,
    pub variant: String,
    pub repeat: u32,
    pub passed: bool,
    /// `agent86`'s exit code; `None` when it was killed.
    pub agent_exit: Option<i32>,
    pub timed_out: bool,
    pub steps: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub compactions: u64,
    /// `context_trim` events in the run's trace.
    pub context_trims: u64,
    pub duration_s: f64,
    /// What went wrong outside the check (a setup failure, an agent error).
    pub error: Option<String>,
    pub check_tail: String,
    #[serde(rename = "final")]
    pub final_answer: String,
}

/// Merge `from` into `into`, tables recursively, every other value replaced.
pub fn merge(into: &mut Table, from: &Table) {
    for (key, item) in from.iter() {
        match (into.get_mut(key), item) {
            (Some(Item::Table(existing)), Item::Table(incoming)) => merge(existing, incoming),
            _ => {
                into.insert(key, item.clone());
            }
        }
    }
}

/// The `[providers]` and `[pricing]` tables of a config file's text, as TOML,
/// so a run reaches the user's servers (a vLLM `base_url`) without their MCP
/// servers, skills, or anything else. Keys never live in config.
pub fn providers_of(config: &str) -> Result<String, String> {
    let doc = config
        .parse::<DocumentMut>()
        .map_err(|error| format!("your config: {error}"))?;
    let mut kept = DocumentMut::new();
    for key in ["providers", "pricing"] {
        if let Some(item) = doc.get(key) {
            kept.insert(key, item.clone());
        }
    }
    Ok(kept.to_string())
}

/// The isolated home's config for `model` running `task` under `variant`: the
/// user's providers, then `evals/config.toml`, then the variant, then what the
/// run sets itself (the model, the task's budgets, `--window`).
pub fn home_config(
    setup: &Setup,
    task: &Task,
    model: &str,
    variant: &Variant,
) -> Result<String, String> {
    let mut doc = DocumentMut::new();
    for (name, text) in [
        ("your providers", setup.providers.as_str()),
        ("evals/config.toml", setup.eval_config.as_str()),
        (variant.name.as_str(), variant.overlay.as_str()),
    ] {
        let layer = text
            .parse::<DocumentMut>()
            .map_err(|error| format!("{name}: {error}"))?;
        merge(doc.as_table_mut(), layer.as_table());
    }
    let mut run = DocumentMut::new();
    run["model"] = Item::Table(Table::new());
    run["model"]["default"] = value(model);
    run["limits"] = Item::Table(Table::new());
    run["limits"]["max_wall_clock_s"] =
        value(i64::try_from(task.file.timeout_s).unwrap_or(i64::MAX));
    if let Some(steps) = task.file.max_steps {
        run["limits"]["max_steps"] = value(i64::from(steps));
    }
    if let Some(window) = setup.window {
        let mut windows = Table::new();
        windows.insert(model, value(i64::try_from(window).unwrap_or(i64::MAX)));
        run["model"]["context_window"] = Item::Table(windows);
    }
    merge(doc.as_table_mut(), run.as_table());
    Ok(doc.to_string())
}

/// A name safe in a path: anything but ASCII letters, digits, `.`, and `-` as `_`.
pub fn slug(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn scratch_dir(setup: &Setup) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    setup.scratch.join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        let head: String = text.chars().take(limit).collect();
        format!("{head}...")
    }
}

/// Count the trace's events of `kind`.
fn count_kind(trace: &str, kind: &str) -> u64 {
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["kind"] == kind)
        .count() as u64
}

/// The `agent86` command for one run: `--approval auto`, so writes and commands
/// run without a terminal to ask at (inside the sandbox executor, in a throwaway
/// workspace), and every `AGENT86_*` variable of the caller removed so nothing
/// but the isolated home configures it.
fn agent_command(setup: &Setup, model: &str, goal: &str, workspace: &Path, home: &Path) -> Command {
    let mut command = Command::new(&setup.agent86);
    command
        .args([
            "--model",
            model,
            "--approval",
            "auto",
            "run",
            "--json",
            goal,
        ])
        .current_dir(workspace);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("AGENT86_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("AGENT86_NO_UPDATE_CHECK", "1")
        .env("HF_HUB_OFFLINE", "1");
    command
}

/// Run `task` once on `model` under `variant`, and score it. Never fails: what
/// goes wrong is in the record, and its files are under `out_dir/runs/`.
pub fn run_one(setup: &Setup, task: &Task, model: &str, variant: &Variant, repeat: u32) -> Record {
    let mut record = Record {
        task: task.id.clone(),
        category: task.file.category.clone(),
        model: model.to_owned(),
        variant: variant.name.clone(),
        repeat,
        passed: false,
        agent_exit: None,
        timed_out: false,
        steps: 0,
        tool_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        compactions: 0,
        context_trims: 0,
        duration_s: 0.0,
        error: None,
        check_tail: String::new(),
        final_answer: String::new(),
    };
    let run_dir = setup
        .out_dir
        .join("runs")
        .join(slug(model))
        .join(slug(&variant.name))
        .join(format!("{}-{repeat}", task.id));
    let scratch = scratch_dir(setup);
    if let Err(error) = run_in(setup, task, model, variant, &run_dir, &scratch, &mut record) {
        record.error = Some(error);
    }
    if !setup.keep {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    record
}

fn run_in(
    setup: &Setup,
    task: &Task,
    model: &str,
    variant: &Variant,
    run_dir: &Path,
    scratch: &Path,
    record: &mut Record,
) -> Result<(), String> {
    let io = |error: std::io::Error| error.to_string();
    std::fs::create_dir_all(run_dir).map_err(io)?;
    let workspace = scratch.join("ws");
    let home = scratch.join("home");
    task.prepare(&workspace, &setup.python)?;
    let config_dir = home.join(".agent86");
    std::fs::create_dir_all(&config_dir).map_err(io)?;
    let config = home_config(setup, task, model, variant)?;
    std::fs::write(config_dir.join("config.toml"), &config).map_err(io)?;
    std::fs::write(run_dir.join("config.toml"), &config).map_err(io)?;

    let timeout = Duration::from_secs(task.file.timeout_s) + KILL_GRACE;
    let agent: Outcome = run(
        agent_command(setup, model, &task.file.goal, &workspace, &home),
        timeout,
    )
    .map_err(|error| format!("cannot run {}: {error}", setup.agent86.display()))?;
    std::fs::write(run_dir.join("agent.json"), &agent.stdout).map_err(io)?;
    std::fs::write(run_dir.join("agent.stderr"), &agent.stderr).map_err(io)?;
    record.agent_exit = agent.code;
    record.timed_out = agent.timed_out;
    record.duration_s = (agent.elapsed.as_secs_f64() * 10.0).round() / 10.0;

    let mut final_answer = String::new();
    match serde_json::from_str::<Value>(agent.stdout.trim()) {
        Ok(output) => {
            final_answer = output["final"].as_str().unwrap_or_default().to_owned();
            record.steps = output["steps"].as_u64().unwrap_or(0);
            let turn = &output["turn"];
            record.tool_calls = turn["tool_calls"].as_u64().unwrap_or(0);
            record.input_tokens = turn["input_tokens"].as_u64().unwrap_or(0);
            record.output_tokens = turn["output_tokens"].as_u64().unwrap_or(0);
            record.cost_usd = turn["cost_usd"].as_f64().unwrap_or(0.0);
            record.compactions = turn["compactions"].as_u64().unwrap_or(0);
        }
        Err(_) => {
            record.error = Some(if agent.timed_out {
                "the agent was killed at its time limit".to_owned()
            } else {
                format!("agent86 wrote no result: {}", agent.tail(800))
            });
        }
    }
    // A turn that failed (out of steps, a provider error) exits 1 with the
    // reason on stderr.
    if !agent.success() && record.error.is_none() {
        record.error = Some(agent.tail(800));
    }
    record.final_answer = clip(&final_answer, FINAL_CHARS);
    if let Ok(trace) = std::fs::read_to_string(config_dir.join("traces").join("trace.jsonl")) {
        record.context_trims = count_kind(&trace, "context_trim");
        if record.compactions == 0 {
            record.compactions = count_kind(&trace, "compaction");
        }
        let _ = std::fs::write(run_dir.join("trace.jsonl"), trace);
    }

    // Scored by the files the agent left, whatever happened to the turn.
    let final_file = scratch.join("final.txt");
    std::fs::write(&final_file, &final_answer).map_err(io)?;
    let check = task.check(&workspace, &final_file, &setup.python)?;
    record.passed = check.success();
    record.check_tail = check.tail(1_500);
    std::fs::write(run_dir.join("check.txt"), check.tail(20_000)).map_err(io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskFile;

    fn task(timeout_s: u64, max_steps: Option<u32>) -> Task {
        Task {
            id: "t".into(),
            dir: PathBuf::from("."),
            file: TaskFile {
                goal: "g".into(),
                category: "bugfix".into(),
                measures: String::new(),
                timeout_s,
                max_steps,
                check: vec!["x".into()],
                setup: Vec::new(),
            },
        }
    }

    fn setup(window: Option<u64>) -> Setup {
        Setup {
            agent86: PathBuf::new(),
            python: "python".into(),
            out_dir: PathBuf::new(),
            scratch: PathBuf::new(),
            providers: "[providers.vllm]\nbase_url = \"http://spark:8000/v1\"\n".into(),
            eval_config: "[limits]\nmax_steps = 40\nmax_cost_usd = 2.0\n[memory]\nembeddings = \"hash:256\"\n".into(),
            window,
            keep: false,
        }
    }

    #[test]
    fn only_providers_and_pricing_are_taken_from_the_users_config() {
        let user = "[model]\ndefault = \"x:y\"\n[providers.vllm]\nbase_url = \"u\"\n[pricing.models]\n\"a:b\" = { input = 1.0, output = 2.0 }\n[mcp.servers.fc]\nurl = \"https://x\"\n";
        let kept: toml::Table = toml::from_str(&providers_of(user).unwrap()).unwrap();
        let keys: Vec<&String> = kept.keys().collect();
        assert_eq!(keys, ["providers", "pricing"]);
    }

    #[test]
    fn the_home_config_layers_providers_eval_variant_and_the_run() {
        let variant = Variant {
            name: "drop".into(),
            overlay: "[limits]\ncompaction = \"drop\"\nmax_steps = 20\n".into(),
        };
        let text = home_config(
            &setup(Some(32_768)),
            &task(300, Some(25)),
            "vllm:org/model",
            &variant,
        )
        .unwrap();
        let config: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            config["providers"]["vllm"]["base_url"].as_str(),
            Some("http://spark:8000/v1")
        );
        assert_eq!(config["memory"]["embeddings"].as_str(), Some("hash:256"));
        let limits = &config["limits"];
        assert_eq!(limits["compaction"].as_str(), Some("drop"));
        assert_eq!(limits["max_cost_usd"].as_float(), Some(2.0));
        // The task's own budget wins over the variant's and the eval's.
        assert_eq!(limits["max_steps"].as_integer(), Some(25));
        assert_eq!(limits["max_wall_clock_s"].as_integer(), Some(300));
        assert_eq!(config["model"]["default"].as_str(), Some("vllm:org/model"));
        assert_eq!(
            config["model"]["context_window"]["vllm:org/model"].as_integer(),
            Some(32_768)
        );
        // Without a task budget the variant's stands, and no window is set.
        let text = home_config(&setup(None), &task(300, None), "m:x", &variant).unwrap();
        let config: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(config["limits"]["max_steps"].as_integer(), Some(20));
        assert!(config["model"].get("context_window").is_none());
    }

    #[test]
    fn slugs_and_trace_counts() {
        assert_eq!(slug("vllm:org/Qwen3.6-35B"), "vllm_org_Qwen3.6-35B");
        let trace = "{\"kind\":\"compaction\"}\n{\"kind\":\"context_trim\"}\nnot json\n{\"kind\":\"context_trim\"}\n";
        assert_eq!(count_kind(trace, "context_trim"), 2);
        assert_eq!(count_kind(trace, "compaction"), 1);
    }
}
