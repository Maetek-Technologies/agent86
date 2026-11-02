//! `run_command` and `python_exec` under the subprocess sandbox, plus the
//! environment scrubbing and Docker command construction behind them.

mod support;

use agent86::config::SandboxConfig;
use agent86::tools::sandbox::{mount_source, scrubbed_env, DockerExecutor, ExecRequest};
use agent86::tools::{builtin_registry, display_path, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::fixture;

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-exec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn context(&self) -> ToolContext {
        ToolContext::new(&self.root).unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(ctx: &ToolContext, name: &str, arguments: Value) -> ToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("arguments must be an object");
    };
    let call = ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments,
    };
    builtin_registry().dispatch(&call, ctx)
}

fn command(ctx: &ToolContext, line: &str) -> ToolResult {
    run(ctx, "run_command", json!({ "command": line }))
}

#[test]
fn specs_match_pydantic_and_registration_order() {
    let expected: Value = serde_json::from_str(&fixture("tools/exec_tools.json")).unwrap();
    let registry = builtin_registry();
    let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(
        names,
        [
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "run_command",
            "python_exec",
            "web_fetch",
        ]
    );
    for name in ["run_command", "python_exec"] {
        let tool = registry.get(name).unwrap();
        assert!(tool.side_effecting(), "{name}");
        assert_eq!(
            serde_json::to_string(&tool.parameters()).unwrap(),
            serde_json::to_string(&expected[name]).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn commands_report_exit_code_stdout_and_stderr() {
    let workspace = Workspace::new("basic");
    let ctx = workspace.context();

    let hello = command(&ctx, "echo hello");
    assert!(hello.ok, "{hello:?}");
    assert_eq!(hello.content, "exit code: 0\nstdout:\nhello");
    assert_eq!(hello.metadata["returncode"], 0);
    assert_eq!(hello.metadata["timed_out"], false);

    let failed = command(&ctx, "exit 3");
    assert!(!failed.ok);
    assert_eq!(failed.content, "exit code: 3");
    assert_eq!(failed.error, None);

    let stderr = command(&ctx, "echo oops 1>&2");
    assert_eq!(stderr.content, "exit code: 0\nstderr:\noops");
}

#[test]
fn commands_run_in_the_workspace() {
    let workspace = Workspace::new("cwd");
    let ctx = workspace.context();

    let line = if cfg!(windows) { "cd" } else { "pwd" };
    let result = command(&ctx, line);

    let expected = display_path(ctx.policy.workspace());
    let printed = result.content.lines().last().unwrap().to_owned();
    // macOS resolves /var to /private/var; compare canonical forms.
    let canonical = |text: &str| std::fs::canonicalize(text).map(|path| display_path(&path));
    assert_eq!(
        canonical(&printed).unwrap(),
        canonical(&expected).unwrap(),
        "{result:?}"
    );
}

#[test]
fn the_environment_is_scrubbed_of_secrets() {
    let environment: Vec<(OsString, OsString)> = [
        ("PATH", "/bin"),
        ("OPENROUTER_API_KEY", "sk-or-secret"),
        ("GITHUB_TOKEN", "ghp_secret"),
        ("RANDOM_SETTING", "x"),
        ("LC_TIME", "C"),
        ("LC_SECRET", "nope"),
        ("FOO", "bar"),
    ]
    .iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect();

    let env = scrubbed_env(&["FOO".to_owned(), "GITHUB_TOKEN".to_owned()], environment);
    let names: Vec<String> = env
        .keys()
        .map(|name| name.to_string_lossy().into_owned())
        .collect();

    assert!(names.contains(&"PATH".to_owned()), "{names:?}");
    assert!(names.contains(&"LC_TIME".to_owned()), "{names:?}");
    assert!(names.contains(&"FOO".to_owned()), "passthrough: {names:?}");
    for leaked in [
        "OPENROUTER_API_KEY",
        "GITHUB_TOKEN",
        "RANDOM_SETTING",
        "LC_SECRET",
    ] {
        assert!(
            !names.contains(&leaked.to_owned()),
            "{leaked} leaked: {names:?}"
        );
    }
    assert_eq!(env[&OsString::from("PYTHONIOENCODING")], "utf-8");
}

#[cfg(windows)]
#[test]
fn windows_environment_names_match_case_insensitively() {
    let environment = vec![
        (OsString::from("Path"), OsString::from("C:\\Windows")),
        (OsString::from("SystemRoot"), OsString::from("C:\\Windows")),
    ];
    let env = scrubbed_env(&[], environment);
    assert!(env.contains_key(&OsString::from("PATH")));
    assert!(env.contains_key(&OsString::from("SYSTEMROOT")));
}

#[test]
fn secrets_in_the_parent_environment_never_reach_a_command() {
    let workspace = Workspace::new("env");
    let ctx = workspace.context();
    std::env::set_var("AGENT86_LEAK_CHECK_API_KEY", "should-not-leak");

    let listing = command(&ctx, if cfg!(windows) { "set" } else { "env" });

    std::env::remove_var("AGENT86_LEAK_CHECK_API_KEY");
    assert!(listing.ok, "{listing:?}");
    assert!(!listing.content.contains("AGENT86_LEAK_CHECK_API_KEY"));
    assert!(!listing.content.contains("should-not-leak"));
    assert!(listing.content.to_uppercase().contains("PATH="));
}

#[test]
fn slow_commands_are_killed_at_the_timeout() {
    let workspace = Workspace::new("timeout");
    let mut ctx = workspace.context();
    ctx.policy.timeout = Duration::from_secs(1);
    let line = if cfg!(windows) {
        "ping -n 8 127.0.0.1 >nul"
    } else {
        "sleep 8"
    };

    let started = Instant::now();
    let result = command(&ctx, line);

    assert!(
        started.elapsed() < Duration::from_secs(6),
        "took {:?}",
        started.elapsed()
    );
    assert!(!result.ok);
    assert_eq!(result.metadata["returncode"], 124);
    assert_eq!(result.metadata["timed_out"], true);
    assert!(
        result.content.starts_with("exit code: 124\nstderr:\n"),
        "{result:?}"
    );
    assert!(
        result.content.ends_with("[timed out after 1s]"),
        "{result:?}"
    );
}

#[cfg(unix)]
#[test]
fn the_timeout_kills_background_children_too() {
    let workspace = Workspace::new("tree");
    let mut ctx = workspace.context();
    ctx.policy.timeout = Duration::from_secs(1);

    // The backgrounded sleep holds stdout open; only a tree kill releases it.
    let started = Instant::now();
    let result = command(&ctx, "sleep 30 & sleep 30");

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(result.metadata["timed_out"], true);
}

#[test]
fn python_snippets_run_from_stdin() {
    let workspace = Workspace::new("python");
    let ctx = workspace.context();

    let printed = run(
        &ctx,
        "python_exec",
        json!({"code": "total = 0\nfor i in range(4):\n    total += i\nprint(total)\n"}),
    );
    assert!(printed.ok, "{printed:?}");
    assert_eq!(printed.content, "exit code: 0\nstdout:\n6");

    let failed = run(&ctx, "python_exec", json!({"code": "1 / 0"}));
    assert!(!failed.ok);
    assert!(
        failed
            .content
            .starts_with("exit code: 1\nstderr:\nTraceback"),
        "{failed:?}"
    );
    assert!(
        failed
            .content
            .ends_with("ZeroDivisionError: division by zero"),
        "{failed:?}"
    );

    let exited = run(&ctx, "python_exec", json!({"code": "raise SystemExit(4)"}));
    assert_eq!(exited.content, "exit code: 4");
}

#[test]
fn previews_show_the_whole_command_up_to_sixty_lines() {
    let workspace = Workspace::new("preview");
    let ctx = workspace.context();
    let registry = builtin_registry();
    let preview = |name: &str, arguments: Value| {
        let Value::Object(arguments) = arguments else {
            unreachable!()
        };
        registry.get(name).unwrap().preview(&arguments, &ctx)
    };

    assert_eq!(
        preview("run_command", json!({"command": "rm -rf build"})).unwrap(),
        "rm -rf build"
    );
    let long: String = (0..65).map(|i| format!("print({i})\n")).collect();
    let snippet = preview("python_exec", json!({ "code": long })).unwrap();
    assert!(
        snippet.ends_with("print(59)\n... [5 more lines]"),
        "{snippet}"
    );
    assert_eq!(preview("run_command", json!({"command": 5})), None);
}

#[test]
fn docker_passes_secrets_by_name_only() {
    // Beyond Python: the value is in the docker client's environment, never argv,
    // so a process list cannot show it.
    let config = SandboxConfig {
        mode: agent86::config::SandboxMode::Docker,
        docker_image: "python:3.12-slim".to_owned(),
        docker_memory: "512m".to_owned(),
        docker_cpus: "1".to_owned(),
        docker_network: false,
        env_passthrough: Vec::new(),
    };
    let argv = DockerExecutor::new(&config).build_command_with_env(
        Path::new("/home/me/proj"),
        &ExecRequest::Shell("env".to_owned()),
        None,
        &["API_TOKEN", "DB_PASSWORD"],
    );
    let image = argv
        .iter()
        .position(|part| part == "python:3.12-slim")
        .unwrap();
    assert_eq!(
        argv[image - 4..image],
        ["-e", "API_TOKEN", "-e", "DB_PASSWORD"]
    );
}

#[test]
fn an_entered_secret_reaches_a_real_process_and_comes_back_redacted() {
    let workspace = Workspace::new("secret-env");
    let ctx = workspace.context();
    ctx.secrets
        .set("AGENT86_TEST_SECRET", "zq-entered-value".to_owned());
    let command = if cfg!(windows) {
        "echo [%AGENT86_TEST_SECRET%]"
    } else {
        "echo [$AGENT86_TEST_SECRET]"
    };
    let result = self::command(&ctx, command);
    assert!(result.ok, "{:?}", result.error);
    assert_eq!(
        result.content,
        "exit code: 0\nstdout:\n[${secret:AGENT86_TEST_SECRET}]"
    );
}

#[test]
fn docker_commands_isolate_the_container() {
    let config = SandboxConfig {
        mode: agent86::config::SandboxMode::Docker,
        docker_image: "python:3.12-slim".to_owned(),
        docker_memory: "512m".to_owned(),
        docker_cpus: "1".to_owned(),
        docker_network: false,
        env_passthrough: Vec::new(),
    };
    let docker = DockerExecutor::new(&config);
    let workspace = if cfg!(windows) {
        Path::new(r"C:\Users\me\proj")
    } else {
        Path::new("/home/me/proj")
    };

    let argv = docker.build_command(
        workspace,
        &ExecRequest::Shell("ls -la".to_owned()),
        Some("agent86-abc"),
    );

    let mount = if cfg!(windows) {
        "/c/Users/me/proj:/workspace"
    } else {
        "/home/me/proj:/workspace"
    };
    assert_eq!(
        argv,
        [
            "docker",
            "run",
            "--rm",
            "-i",
            "--memory",
            "512m",
            "--cpus",
            "1",
            "--pids-limit",
            "256",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CHOWN",
            "--cap-add",
            "DAC_OVERRIDE",
            "--cap-add",
            "FOWNER",
            "--cap-add",
            "FSETID",
            "--cap-add",
            "SETUID",
            "--cap-add",
            "SETGID",
            "--security-opt",
            "no-new-privileges",
            "-w",
            "/workspace",
            "-v",
            mount,
            "--network",
            "none",
            "--name",
            "agent86-abc",
            "python:3.12-slim",
            "sh",
            "-c",
            "ls -la",
        ]
    );

    let networked = DockerExecutor {
        network: true,
        ..docker
    };
    let python = networked.build_command(
        workspace,
        &ExecRequest::Argv(vec!["python".into(), "-I".into(), "-".into()]),
        None,
    );
    assert!(!python.contains(&"--network".to_owned()));
    assert!(python.ends_with(&[
        "python:3.12-slim".to_owned(),
        "python".to_owned(),
        "-I".to_owned(),
        "-".to_owned()
    ]));
}

#[cfg(windows)]
#[test]
fn windows_drives_become_docker_mount_paths() {
    assert_eq!(mount_source(Path::new(r"D:\work\repo")), "/d/work/repo");
    assert_eq!(mount_source(Path::new(r"\\?\C:\Users\me")), "/c/Users/me");
}

#[cfg(unix)]
#[test]
fn posix_paths_mount_as_is() {
    assert_eq!(mount_source(Path::new("/srv/repo")), "/srv/repo");
}
