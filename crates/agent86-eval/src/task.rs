//! Tasks: what each one asks, the files it starts from, and the hidden check
//! that scores it.
//!
//! A task is a directory under `evals/tasks/<id>/`:
//!
//! - `task.toml`: [`TaskFile`].
//! - `workspace/`: copied into a fresh directory, which is all the agent sees.
//! - `setup/` (optional): scripts `setup` runs in that directory before the
//!   agent starts, for fixtures too large to keep in the repository. Never copied.
//! - `check/`: copied over the workspace only after the agent has finished, so the
//!   agent can neither read nor edit it; then `check` runs there.
//! - `solution/`: a reference fix, copied over a fresh workspace by
//!   `agent86-eval validate` to show the check can pass. `solution/eval_final.txt`,
//!   when present, stands in for the agent's final answer, and
//!   `solution/eval_delete.txt` lists workspace paths (one per line) the
//!   reference fix removes.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{copy_dir, run_argv, Outcome};

/// The final answer's stand-in in `solution/`.
pub const SOLUTION_FINAL: &str = "eval_final.txt";

/// The paths a reference fix removes, in `solution/`.
pub const SOLUTION_DELETE: &str = "eval_delete.txt";

/// The variable a check reads the agent's final answer from (a path).
pub const FINAL_VAR: &str = "EVAL_FINAL_FILE";

/// How long a check or a setup step may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// `task.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFile {
    /// What the agent is asked, word for word.
    pub goal: String,
    /// What kind of task it is, which the summary groups by (`bugfix`,
    /// `feature`, `refactor`, `long-context`, `search`, `large-output`,
    /// `verification`, `stall`).
    pub category: String,
    /// Which slice of the local-model work it is meant to show (free text).
    #[serde(default)]
    pub measures: String,
    /// Wall-clock budget for the agent, in seconds.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// Step budget for the agent; the harness default when unset.
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// The check, as an argument list run in the workspace (no shell).
    /// `{python}` is replaced by the Python found on PATH.
    pub check: Vec<String>,
    /// Run in the workspace before the agent starts; `{python}` as above,
    /// `{setup}` the task's `setup/` directory.
    #[serde(default)]
    pub setup: Vec<String>,
}

fn default_timeout_s() -> u64 {
    600
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub dir: PathBuf,
    pub file: TaskFile,
}

/// Every task under `root`, in id order.
pub fn load_tasks(root: &Path) -> Result<Vec<Task>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("cannot read tasks in {}: {error}", root.display()))?;
    let mut tasks = Vec::new();
    for entry in entries {
        let dir = entry.map_err(|error| error.to_string())?.path();
        let manifest = dir.join("task.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)
            .map_err(|error| format!("{}: {error}", manifest.display()))?;
        let file: TaskFile =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", manifest.display()))?;
        if file.check.is_empty() {
            return Err(format!("{}: `check` is empty", manifest.display()));
        }
        let id = dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{}: the directory name is not UTF-8", dir.display()))?
            .to_owned();
        tasks.push(Task { id, dir, file });
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

/// The tasks `ids` and `categories` select (all, when both are empty).
pub fn select(
    tasks: Vec<Task>,
    ids: &[String],
    categories: &[String],
) -> Result<Vec<Task>, String> {
    for id in ids {
        if !tasks.iter().any(|task| &task.id == id) {
            return Err(format!("no task named '{id}'"));
        }
    }
    Ok(tasks
        .into_iter()
        .filter(|task| {
            (ids.is_empty() && categories.is_empty())
                || ids.contains(&task.id)
                || categories.contains(&task.file.category)
        })
        .collect())
}

/// The Python on PATH, as the MCP tests find it: `python3`, else `python`.
pub fn find_python() -> Option<String> {
    ["python3", "python"].into_iter().find_map(|name| {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| name.to_owned())
    })
}

impl Task {
    fn expand(&self, argv: &[String], python: &str) -> Vec<String> {
        let setup = self.dir.join("setup");
        argv.iter()
            .map(|arg| {
                arg.replace("{python}", python)
                    .replace("{setup}", &setup.display().to_string())
            })
            .collect()
    }

    /// A fresh copy of the starting workspace at `dest`, with setup run in it.
    pub fn prepare(&self, dest: &Path, python: &str) -> Result<(), String> {
        let _ = std::fs::remove_dir_all(dest);
        std::fs::create_dir_all(dest).map_err(|error| format!("{}: {error}", dest.display()))?;
        copy_dir(&self.dir.join("workspace"), dest)?;
        if !self.file.setup.is_empty() {
            let outcome = run_argv(
                &self.expand(&self.file.setup, python),
                dest,
                &[],
                CHECK_TIMEOUT,
            )?;
            if !outcome.success() {
                return Err(format!(
                    "task '{}': setup failed: {}",
                    self.id,
                    outcome.tail(2_000)
                ));
            }
        }
        Ok(())
    }

    /// Copy the hidden check over `workspace` and run it, with the final answer
    /// in `final_file`.
    pub fn check(
        &self,
        workspace: &Path,
        final_file: &Path,
        python: &str,
    ) -> Result<Outcome, String> {
        copy_dir(&self.dir.join("check"), workspace)?;
        run_argv(
            &self.expand(&self.file.check, python),
            workspace,
            &[(FINAL_VAR, final_file.display().to_string())],
            CHECK_TIMEOUT,
        )
    }
}

/// Why a task failed validation, if it did: its check must fail on the
/// untouched workspace (a do-nothing agent scores nothing) and pass with the
/// reference solution.
pub fn validate(task: &Task, scratch: &Path, python: &str) -> Result<(), String> {
    let workspace = scratch.join(format!("{}-pristine", task.id));
    let final_file = scratch.join(format!("{}-pristine.final", task.id));
    task.prepare(&workspace, python)?;
    std::fs::write(&final_file, "").map_err(|error| error.to_string())?;
    let pristine = task.check(&workspace, &final_file, python)?;
    if pristine.success() {
        return Err(format!(
            "task '{}': the check passes on the untouched workspace",
            task.id
        ));
    }

    let workspace = scratch.join(format!("{}-solved", task.id));
    let final_file = scratch.join(format!("{}-solved.final", task.id));
    task.prepare(&workspace, python)?;
    let solution = task.dir.join("solution");
    copy_dir(&solution, &workspace)?;
    let stand_in = workspace.join(SOLUTION_FINAL);
    let answer = std::fs::read_to_string(&stand_in).unwrap_or_default();
    let _ = std::fs::remove_file(&stand_in);
    let deletions = workspace.join(SOLUTION_DELETE);
    let doomed = std::fs::read_to_string(&deletions).unwrap_or_default();
    let _ = std::fs::remove_file(&deletions);
    for relative in doomed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = workspace.join(relative);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .map_err(|error| format!("task '{}': {SOLUTION_DELETE}: {relative}: {error}", task.id))?;
    }
    std::fs::write(&final_file, answer).map_err(|error| error.to_string())?;
    let solved = task.check(&workspace, &final_file, python)?;
    if !solved.success() {
        return Err(format!(
            "task '{}': the check fails with the reference solution: {}",
            task.id,
            solved.tail(2_000)
        ));
    }
    let _ = std::fs::remove_dir_all(scratch.join(format!("{}-pristine", task.id)));
    let _ = std::fs::remove_dir_all(&workspace);
    Ok(())
}
