//! Git worktrees for isolated specialist sub-agents (beyond Python;
//! `docs/subagents.md` section 7).
//!
//! A specialty with `isolation: worktree` (or a `delegate` call with `isolate:
//! true`) runs in a `git worktree` of the workspace's `HEAD` in the temporary
//! directory, jailed there instead of the real workspace. Its result carries the
//! worktree's diff against that base; nothing is merged automatically. A worktree
//! with changes is kept (its path and branch are reported, so the work is not
//! lost); one without is removed. A startup sweep removes the worktrees of
//! processes that have exited, never another live process's, and never one that
//! was kept on purpose.
//!
//! agent86's own `git` calls on the host are hardened: hooks point at an empty
//! directory (`core.hooksPath`), `core.fsmonitor` is off, the diff runs with
//! `--no-ext-diff --no-textconv`, and after creation every call names the
//! worktree's git directory and work tree explicitly, so sandboxed code that
//! rewrites the worktree's `.git` file cannot redirect them. A filter driver
//! (`filter.<name>.smudge`) configured in the repository would still run on
//! checkout: that is a known limit, since the user's own repository config is
//! trusted as git trusts it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every worktree directory agent86 makes starts with this, then the owning
/// process id: `agent86-wt-<pid>-<n>-<nanos>`.
pub const PREFIX: &str = "agent86-wt-";

/// Beside a kept worktree's directory: the sweep leaves it alone.
const KEPT_SUFFIX: &str = ".kept";

/// The diff handed back is cut at this many characters, saying so.
pub const MAX_DIFF_CHARS: usize = 60_000;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// One isolated worktree.
#[derive(Debug)]
pub struct Worktree {
    /// The worktree's own root.
    pub path: PathBuf,
    /// Where the sub-agent is jailed: the workspace's place inside the worktree
    /// (the workspace may be a subdirectory of the repository).
    pub root: PathBuf,
    /// The branch it was made on, `agent86/<name>`.
    pub branch: String,
    /// The commit it started from.
    pub base: String,
    /// The real workspace had uncommitted changes the worktree does not have.
    pub dirty: bool,
    /// The repository's common git directory.
    common: PathBuf,
    /// The worktree's own git directory (`<common>/worktrees/<name>`).
    git_dir: PathBuf,
}

/// An empty directory for `core.hooksPath`, so no hook runs.
fn empty_hooks() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("agent86-wt-hooks-empty");
    std::fs::create_dir_all(&dir).map_err(|error| format!("could not prepare git: {error}"))?;
    Ok(dir)
}

/// A hardened `git` command: no hooks, no fsmonitor, and none of the environment
/// variables that would point it at another repository.
fn git(hooks: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            crate::tools::plain_path(hooks).display()
        ))
        .args(["-c", "core.fsmonitor=false", "-c", "color.ui=false"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// Run `command` and return its trimmed stdout, or its stderr as the error.
fn output(mut command: Command) -> Result<String, String> {
    let out = command
        .output()
        .map_err(|error| format!("git could not be run: {error}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(stderr
            .trim()
            .lines()
            .last()
            .unwrap_or("git failed")
            .to_owned())
    }
}

fn canonical(path: &Path) -> PathBuf {
    crate::tools::plain_path(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()))
}

impl Worktree {
    /// A worktree of `workspace`'s repository at `HEAD`, in the temporary
    /// directory. Outside a repository, or in one with no commits, it fails with a
    /// clear message: there is no silent fallback to the shared workspace.
    pub fn create(workspace: &Path) -> Result<Self, String> {
        let hooks = empty_hooks()?;
        let workspace = canonical(workspace);
        let in_workspace = |args: &[&str]| {
            let mut command = git(&hooks);
            command.arg("-C").arg(&workspace).args(args);
            output(command)
        };
        let not_a_repo = |_| {
            "Isolation needs a git repository: the workspace is not inside one, so the \
             specialty was not run (rather than run in the shared workspace)."
                .to_owned()
        };
        let top = canonical(Path::new(
            &in_workspace(&["rev-parse", "--show-toplevel"]).map_err(not_a_repo)?,
        ));
        let common = canonical(Path::new(
            &in_workspace(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .map_err(not_a_repo)?,
        ));
        let base = in_workspace(&["rev-parse", "--verify", "HEAD"]).map_err(|_| {
            "Isolation needs a commit to start from, and this repository has none yet.".to_owned()
        })?;
        let dirty = in_workspace(&["status", "--porcelain"])
            .map(|status| !status.is_empty())
            .unwrap_or(false);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!(
            "{PREFIX}{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(&name);
        let branch = format!("agent86/{name}");
        let mut add = git(&hooks);
        add.arg("--git-dir")
            .arg(&common)
            .args(["worktree", "add", "--quiet", "-b", &branch])
            .arg(crate::tools::plain_path(&path))
            .arg(&base);
        output(add).map_err(|error| format!("The worktree could not be made: {error}"))?;
        let path = canonical(&path);
        let relative = workspace
            .strip_prefix(&top)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let root = path.join(relative);
        if !root.is_dir() {
            // The workspace is a directory with nothing committed in it yet.
            let _ = std::fs::create_dir_all(&root);
        }
        Ok(Self {
            git_dir: common.join("worktrees").join(&name),
            path,
            root,
            branch,
            base,
            dirty,
            common,
        })
    }

    /// A git command on this worktree, naming its git directory and work tree.
    fn command(&self) -> Result<Command, String> {
        let mut command = git(&empty_hooks()?);
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.path)
            .arg("-C")
            .arg(&self.path);
        Ok(command)
    }

    /// Everything the sub-agent changed against the base, untracked files
    /// included (`git add -N` first), without external diff or textconv drivers.
    pub fn diff(&self) -> Result<String, String> {
        let mut add = self.command()?;
        add.args(["add", "--intent-to-add", "--all", "."]);
        output(add)?;
        let mut diff = self.command()?;
        diff.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            &self.base,
        ]);
        output(diff)
    }

    /// Take the worktree and its branch away.
    pub fn remove(&self) {
        if let Ok(hooks) = empty_hooks() {
            let mut remove = git(&hooks);
            remove
                .arg("--git-dir")
                .arg(&self.common)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path);
            let _ = output(remove);
            let mut branch = git(&hooks);
            branch
                .arg("--git-dir")
                .arg(&self.common)
                .args(["branch", "-D", &self.branch]);
            let _ = output(branch);
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }

    /// Mark the worktree as kept on purpose, so no sweep removes it.
    pub fn keep(&self) {
        let mut marker = self.path.clone().into_os_string();
        marker.push(KEPT_SUFFIX);
        let _ = std::fs::write(marker, &self.branch);
    }

    /// What the sub-agent's answer gets after it ran here: the base, the dirty
    /// note, and the diff (redacted by the caller), or that nothing changed. A
    /// worktree with changes is kept; one without is removed.
    pub fn finish(&self, redact: impl Fn(&str) -> String) -> String {
        let short: String = self.base.chars().take(12).collect();
        let mut text = format!(
            "\n\n[worktree] This ran in a git worktree of HEAD ({short}), not in your workspace."
        );
        if self.dirty {
            text.push_str(
                " The workspace had uncommitted changes, which the worktree did not have.",
            );
        }
        match self.diff() {
            Ok(diff) if diff.trim().is_empty() => {
                text.push_str(" It changed nothing, and the worktree was removed.");
                self.remove();
            }
            Ok(diff) => {
                self.keep();
                let diff = redact(&diff);
                let total = diff.chars().count();
                let shown: String = diff.chars().take(MAX_DIFF_CHARS).collect();
                text.push_str(&format!(
                    " Nothing was applied: the worktree is kept at {} on branch {}. Its changes:\n```diff\n{shown}\n```",
                    crate::tools::display_path(&self.path),
                    self.branch
                ));
                if total > MAX_DIFF_CHARS {
                    text.push_str(&format!(
                        "\n[diff cut at {MAX_DIFF_CHARS} of {total} characters; the rest is in the worktree]"
                    ));
                }
            }
            Err(error) => {
                self.keep();
                text.push_str(&format!(
                    " Its diff could not be made ({error}); the worktree is kept at {} on branch {}.",
                    crate::tools::display_path(&self.path),
                    self.branch
                ));
            }
        }
        text
    }
}

/// Whether process `pid` is running.
fn alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let result = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|out| out.status.success())
    };
    // When it cannot be told, the worktree is left alone.
    result.unwrap_or(true)
}

/// Remove the worktrees in `dir` that agent86 processes left behind: only those
/// whose owning process has exited and that were not kept on purpose.
pub fn sweep_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let Some(pid) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() || dir.join(format!("{name}{KEPT_SUFFIX}")).exists() || alive(pid) {
            continue;
        }
        // Tell its repository, through the `.git` file the worktree holds, then
        // remove what is left.
        let gitdir = std::fs::read_to_string(path.join(".git"))
            .ok()
            .and_then(|text| {
                text.strip_prefix("gitdir:")
                    .map(|g| PathBuf::from(g.trim()))
            });
        if let (Some(gitdir), Ok(hooks)) = (gitdir, empty_hooks()) {
            if let Some(common) = gitdir.parent().and_then(Path::parent) {
                let mut remove = git(&hooks);
                remove
                    .arg("--git-dir")
                    .arg(common)
                    .args(["worktree", "remove", "--force"])
                    .arg(&path);
                let _ = output(remove);
                let mut branch = git(&hooks);
                branch.arg("--git-dir").arg(common).args([
                    "branch",
                    "-D",
                    &format!("agent86/{name}"),
                ]);
                let _ = output(branch);
            }
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// [`sweep_in`] the temporary directory, where worktrees are made.
pub fn sweep() {
    sweep_in(&std::env::temp_dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_spares_live_and_kept_worktrees() {
        let dir = std::env::temp_dir().join(format!("agent86-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A pid that cannot be running (above any real limit), ours, and a kept one.
        let dead = dir.join(format!("{PREFIX}4294967290-0-1"));
        let mine = dir.join(format!("{PREFIX}{}-0-1", std::process::id()));
        let kept = dir.join(format!("{PREFIX}4294967291-0-1"));
        let other = dir.join("unrelated");
        for path in [&dead, &mine, &kept, &other] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            dir.join(format!("{PREFIX}4294967291-0-1{KEPT_SUFFIX}")),
            "b",
        )
        .unwrap();
        sweep_in(&dir);
        assert!(!dead.exists());
        assert!(mine.exists() && kept.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
