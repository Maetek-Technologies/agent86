//! Execution backends for command and Python tools, ported from Python's
//! `tools/sandbox/policy.py`, `subprocess_exec.py`, `docker_exec.py`, and
//! `executor.py`.
//!
//! The default restricted subprocess runs with the workspace as its working
//! directory, an allowlisted environment (so API keys never reach tool
//! processes), a hard timeout that kills the whole process tree, and capped
//! output. It is not true isolation; the opt-in Docker backend runs each command
//! in a throwaway container with no network, capped memory and CPU, and a PID limit.

use super::SandboxPolicy;
use crate::config::SandboxConfig;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Seconds to wait for the pipes to drain after a kill before giving up on the output.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Seconds allowed for the `docker kill` that cleans up after a timeout.
const DOCKER_KILL_TIMEOUT: Duration = Duration::from_secs(10);
const CONTAINER_WORKDIR: &str = "/workspace";

/// Environment variables forwarded on every platform.
const ENV_ALLOWLIST_COMMON: &[&str] = &["PATH", "LANG", "LC_ALL", "PYTHONIOENCODING", "PYTHONUTF8"];
const ENV_ALLOWLIST_WINDOWS: &[&str] = &[
    "PATHEXT",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];
/// Without these, git, pip, and npm break on macOS and Linux.
const ENV_ALLOWLIST_POSIX: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];
/// Whole families that are safe one by one (locale categories, XDG directories).
const ENV_ALLOWLIST_PREFIXES: &[&str] = &["LC_", "XDG_"];
/// Substrings that mark a variable as a credential, refused even when passed through.
const SECRET_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "API_KEY",
];
const SECRET_SUFFIXES: &[&str] = &["_KEY"];

/// The exact-name allowlist for this platform.
pub fn env_allowlist() -> Vec<&'static str> {
    let platform = if cfg!(windows) {
        ENV_ALLOWLIST_WINDOWS
    } else {
        ENV_ALLOWLIST_POSIX
    };
    ENV_ALLOWLIST_COMMON
        .iter()
        .chain(platform)
        .copied()
        .collect()
}

/// True if `name` looks like it holds a credential.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    SECRET_MARKERS.iter().any(|marker| upper.contains(marker))
        || SECRET_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// The environment a tool subprocess gets: the allowlist plus opted-in names,
/// never anything credential-shaped.
///
/// Windows environment names are case-insensitive; Python sees them uppercased,
/// so names are matched (and passed on) uppercased there.
pub fn scrubbed_env(
    passthrough: &[String],
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    let normalize = |name: &str| {
        if cfg!(windows) {
            name.to_uppercase()
        } else {
            name.to_owned()
        }
    };
    let current: BTreeMap<String, OsString> = environment
        .into_iter()
        .filter_map(|(name, value)| Some((normalize(name.to_str()?), value)))
        .collect();
    let allowed = env_allowlist();

    let mut env = BTreeMap::new();
    for (name, value) in &current {
        let listed = allowed.contains(&name.as_str());
        let family = ENV_ALLOWLIST_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && !is_secret_name(name);
        if listed || family {
            env.insert(OsString::from(name), value.clone());
        }
    }
    for name in passthrough {
        if is_secret_name(name) {
            // A passthrough entry must not become a way to hand tool processes a key.
            continue;
        }
        if let Some(value) = current.get(&normalize(name)) {
            env.insert(OsString::from(normalize(name)), value.clone());
        }
    }
    env.entry(OsString::from("PYTHONIOENCODING"))
        .or_insert_with(|| OsString::from("utf-8"));
    env
}

/// What a command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl ExecResult {
    pub fn ok(&self) -> bool {
        self.returncode == 0 && !self.timed_out
    }
}

/// What to run: a shell command line, or an argv list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecRequest {
    Shell(String),
    Argv(Vec<String>),
}

/// The configured execution backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executor {
    Subprocess,
    Docker(DockerExecutor),
}

impl Executor {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Subprocess => "subprocess",
            Self::Docker(_) => "docker",
        }
    }

    pub fn run(
        &self,
        policy: &SandboxPolicy,
        request: &ExecRequest,
        stdin: Option<&str>,
    ) -> std::io::Result<ExecResult> {
        match self {
            Self::Subprocess => run_subprocess(policy, request, stdin),
            Self::Docker(docker) => Ok(docker.run(policy, request, stdin)),
        }
    }

    /// The argv that runs a Python snippet read from stdin, isolated from site config.
    ///
    /// Python uses its own interpreter (`sys.executable`); here it is the `python`
    /// on PATH on Windows, where `python3` is often the Microsoft Store stub, and
    /// `python3` elsewhere.
    pub fn python_argv(&self) -> Vec<String> {
        let interpreter = match self {
            Self::Docker(_) => "python",
            Self::Subprocess if cfg!(windows) => "python",
            Self::Subprocess => "python3",
        };
        vec![interpreter.to_owned(), "-I".to_owned(), "-".to_owned()]
    }
}

/// Pick the backend for `config`, with a note when Docker was requested but is unusable.
pub fn build_executor(config: &SandboxConfig) -> (Executor, Option<String>) {
    if config.mode != crate::config::SandboxMode::Docker {
        return (Executor::Subprocess, None);
    }
    if docker_available() {
        return (Executor::Docker(DockerExecutor::new(config)), None);
    }
    (
        Executor::Subprocess,
        Some(
            "Docker requested but not available; falling back to the subprocess sandbox."
                .to_owned(),
        ),
    )
}

/// Whether `docker info` succeeds within ten seconds.
pub fn docker_available() -> bool {
    let Ok(mut child) = Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Decode process output as Python's text-mode pipes do: UTF-8 with replacement
/// and universal newlines.
fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Encode stdin as Python's text-mode pipe does (`\n` becomes `\r\n` on Windows).
fn encode(text: &str) -> Vec<u8> {
    if cfg!(windows) {
        text.replace('\n', "\r\n").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Build the `Command` for a request: `cmd.exe /c "..."` or `/bin/sh -c ...`, as
/// Python's `shell=True` does, or the argv as given.
fn command_for(request: &ExecRequest) -> Command {
    match request {
        ExecRequest::Shell(line) => shell_command(line),
        ExecRequest::Argv(argv) => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        }
    }
}

#[cfg(windows)]
fn shell_command(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
    let mut command = Command::new(comspec);
    // Exactly the command line Python builds: `<comspec> /c "<command>"`.
    command.raw_arg(format!("/c \"{line}\""));
    command
}

#[cfg(not(windows))]
fn shell_command(line: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(line);
    command
}

/// Start the child at the head of its own process group, so the whole tree can be killed.
#[cfg(windows)]
fn new_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn new_process_group(_command: &mut Command) {}

/// Kill `child` and everything it spawned. Best effort, never fails.
fn kill_process_tree(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = child.id().to_string();
    let tree_kill = if cfg!(windows) {
        // A new process group alone does not cascade; taskkill /T walks the tree.
        Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid])
            .output()
    } else {
        // The child leads its own group, whose id is its pid.
        Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .output()
    };
    drop(tree_kill);
    // Make sure the direct child is reaped even if the tree kill missed.
    let _ = child.kill();
}

/// Read `pipe` to the end on a thread, reporting the bytes on `sender`.
fn reader<R: Read + Send + 'static>(
    mut pipe: R,
    sender: mpsc::Sender<(usize, Vec<u8>)>,
    index: usize,
) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        let _ = sender.send((index, bytes));
    });
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(-1)
}

/// Run `request` in a restricted subprocess under `policy`.
///
/// Like Python's `communicate(timeout=...)`, the deadline covers both the process
/// exiting and its pipes closing: a backgrounded grandchild still holding stdout
/// counts as still running.
pub fn run_subprocess(
    policy: &SandboxPolicy,
    request: &ExecRequest,
    stdin: Option<&str>,
) -> std::io::Result<ExecResult> {
    let mut command = command_for(request);
    command
        .current_dir(super::plain_path(policy.workspace()))
        .env_clear()
        .envs(scrubbed_env(&policy.env_passthrough, std::env::vars_os()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    new_process_group(&mut command);
    let mut child = command.spawn()?;

    let input = stdin.map(encode).unwrap_or_default();
    if let Some(mut pipe) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
            // Dropping the pipe closes it, so the child sees end of input.
        });
    }
    let (sender, receiver) = mpsc::channel();
    if let Some(pipe) = child.stdout.take() {
        reader(pipe, sender.clone(), 0);
    }
    if let Some(pipe) = child.stderr.take() {
        reader(pipe, sender, 1);
    }

    let deadline = Instant::now() + policy.timeout;
    let mut outputs: [Option<Vec<u8>>; 2] = [None, None];
    let mut status = None;
    loop {
        while let Ok((index, bytes)) = receiver.try_recv() {
            outputs[index] = Some(bytes);
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let (Some(status), [Some(_), Some(_)]) = (status, &outputs) {
            let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
            return Ok(ExecResult {
                returncode: exit_code(status),
                stdout: policy.truncate(decode(&stdout)),
                stderr: policy.truncate(decode(&stderr)),
                timed_out: false,
            });
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Timed out: kill the tree, then collect whatever it already wrote.
    kill_process_tree(&mut child);
    let drain_deadline = Instant::now() + DRAIN_TIMEOUT;
    while outputs.iter().any(Option::is_none) {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((index, bytes)) => outputs[index] = Some(bytes),
            Err(_) => {
                // A surviving grandchild still holds a pipe: give up on the output.
                outputs = [Some(Vec::new()), Some(Vec::new())];
            }
        }
    }
    let _ = child.wait();
    let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
    let seconds = policy.timeout.as_secs();
    Ok(ExecResult {
        returncode: 124,
        stdout: policy.truncate(decode(&stdout)),
        stderr: format!(
            "{}\n[timed out after {seconds}s]",
            policy.truncate(decode(&stderr))
        ),
        timed_out: true,
    })
}

/// Runs each command in a fresh, throwaway container via the `docker` CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerExecutor {
    pub image: String,
    pub memory: String,
    pub cpus: String,
    pub network: bool,
}

impl DockerExecutor {
    pub fn new(config: &SandboxConfig) -> Self {
        Self {
            image: config.docker_image.clone(),
            memory: config.docker_memory.clone(),
            cpus: config.docker_cpus.clone(),
            network: config.docker_network,
        }
    }

    /// The full `docker run ...` argv for `request`.
    pub fn build_command(
        &self,
        workspace: &Path,
        request: &ExecRequest,
        name: Option<&str>,
    ) -> Vec<String> {
        let mut command: Vec<String> = [
            "docker",
            "run",
            "--rm",
            "-i",
            "--memory",
            &self.memory,
            "--cpus",
            &self.cpus,
            "--pids-limit",
            "256",
            "-w",
            CONTAINER_WORKDIR,
            "-v",
        ]
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
        command.push(format!("{}:{CONTAINER_WORKDIR}", mount_source(workspace)));
        if !self.network {
            command.extend(["--network".to_owned(), "none".to_owned()]);
        }
        if let Some(name) = name {
            // The only handle on the container once `docker run` is killed.
            command.extend(["--name".to_owned(), name.to_owned()]);
        }
        command.push(self.image.clone());
        match request {
            ExecRequest::Shell(line) => {
                command.extend(["sh".to_owned(), "-c".to_owned(), line.clone()]);
            }
            ExecRequest::Argv(argv) => command.extend(argv.iter().cloned()),
        }
        command
    }

    fn run(
        &self,
        policy: &SandboxPolicy,
        request: &ExecRequest,
        stdin: Option<&str>,
    ) -> ExecResult {
        let name = format!("agent86-{}", crate::turn::new_session_id());
        let argv = self.build_command(policy.workspace(), request, Some(&name));
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ExecResult {
                    returncode: 127,
                    stdout: String::new(),
                    stderr: format!("docker not found: {error}"),
                    timed_out: false,
                };
            }
        };
        let input = stdin.map(encode).unwrap_or_default();
        if let Some(mut pipe) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        let (sender, receiver) = mpsc::channel();
        if let Some(pipe) = child.stdout.take() {
            reader(pipe, sender.clone(), 0);
        }
        if let Some(pipe) = child.stderr.take() {
            reader(pipe, sender, 1);
        }

        let deadline = Instant::now() + policy.timeout;
        let mut outputs: [Option<Vec<u8>>; 2] = [None, None];
        loop {
            while let Ok((index, bytes)) = receiver.try_recv() {
                outputs[index] = Some(bytes);
            }
            if let (Ok(Some(status)), [Some(_), Some(_)]) = (child.try_wait(), &outputs) {
                let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
                return ExecResult {
                    returncode: exit_code(status),
                    stdout: policy.truncate(decode(&stdout)),
                    stderr: policy.truncate(decode(&stderr)),
                    timed_out: false,
                };
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Killing the `docker run` client leaves the container running.
        let _ = child.kill();
        let _ = child.wait();
        let killed = kill_container(&name);
        let note = if killed {
            ""
        } else {
            " (container may still be running)"
        };
        ExecResult {
            returncode: 124,
            stdout: String::new(),
            stderr: format!("[timed out after {}s]{note}", policy.timeout.as_secs()),
            timed_out: true,
        }
    }
}

/// Stop an orphaned container by name. Best effort.
fn kill_container(name: &str) -> bool {
    let Ok(mut child) = Command::new("docker")
        .args(["kill", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + DOCKER_KILL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

/// A bind-mount source Docker accepts: `C:\Users\me\proj` becomes `/c/Users/me/proj`,
/// avoiding the drive colon that would confuse `-v src:dst`.
pub fn mount_source(workspace: &Path) -> String {
    let text = super::display_path(workspace).replace('\\', "/");
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        format!("/{}{}", (bytes[0] as char).to_ascii_lowercase(), &text[2..])
    } else {
        text
    }
}
