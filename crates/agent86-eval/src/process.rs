//! Running processes with a deadline, and copying directories.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How a process ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The exit code; `None` when it was killed or died by a signal.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Outcome {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// The last `limit` characters of stderr then stdout, for a report.
    pub fn tail(&self, limit: usize) -> String {
        let both = format!("{}\n{}", self.stderr.trim(), self.stdout.trim());
        let both = both.trim();
        let count = both.chars().count();
        if count <= limit {
            both.to_owned()
        } else {
            let tail: String = both.chars().skip(count - limit).collect();
            format!("...{tail}")
        }
    }
}

/// Run `argv` in `cwd` with `env` added, killed at `timeout`.
pub fn run_argv(
    argv: &[String],
    cwd: &Path,
    env: &[(&str, String)],
    timeout: Duration,
) -> Result<Outcome, String> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
    run(command, timeout).map_err(|error| format!("cannot run {}: {error}", argv[0]))
}

/// Run `command` with its output captured, killed at `timeout`. Only the process
/// itself is killed; the agent's own commands have their own timeouts.
pub fn run(mut command: Command, timeout: Duration) -> std::io::Result<Outcome> {
    let started = Instant::now();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Outcome {
        code: status.and_then(|status| status.code()),
        timed_out,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        elapsed: started.elapsed(),
    })
}

/// Copy everything under `from` into `to`, overwriting files of the same name.
/// A missing `from` copies nothing.
pub fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|error| format!("{}: {error}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|error| format!("{}: {error}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            copy_dir(&source, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target)
                .map_err(|error| format!("{}: {error}", source.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_keep_the_end() {
        let outcome = Outcome {
            code: Some(1),
            timed_out: false,
            stdout: "abcdef".into(),
            stderr: String::new(),
            elapsed: Duration::ZERO,
        };
        assert_eq!(outcome.tail(100), "abcdef");
        assert_eq!(outcome.tail(3), "...def");
        assert!(!outcome.success());
    }

    #[test]
    fn copies_nest_and_overwrite() {
        let root = std::env::temp_dir().join(format!("agent86-eval-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/sub")).unwrap();
        std::fs::write(root.join("a/sub/x.txt"), "new").unwrap();
        std::fs::create_dir_all(root.join("b/sub")).unwrap();
        std::fs::write(root.join("b/sub/x.txt"), "old").unwrap();
        copy_dir(&root.join("a"), &root.join("b")).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("b/sub/x.txt")).unwrap(),
            "new"
        );
        copy_dir(&root.join("missing"), &root.join("b")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
