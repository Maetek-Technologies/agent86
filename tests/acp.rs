//! End-to-end tests of `agent86 acp`: the real binary, driven over piped stdio in
//! an isolated home and project directory, with no network or credentials.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-acp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home").join(".agent86")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent86"))
            .args(args)
            .current_dir(self.root.join("project"))
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env_remove("AGENT86_MODEL")
            .env_remove("AGENT86_SANDBOX")
            .env_remove("AGENT86_APPROVAL")
            .env("AGENT86_TRUST_PROJECT", "1")
            .env("AGENT86_NO_UPDATE_CHECK", "1")
            .env_remove("NO_COLOR")
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Every stdout line, parsed: the protocol forbids any line that is not a message.
fn messages(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"))
        })
        .collect()
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

#[test]
fn the_handshake_works_and_stdout_holds_only_protocol_messages() {
    let workspace = Workspace::new("handshake");
    let input = format!("{INITIALIZE}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{{}}}}\n");
    let output = workspace.run(&["acp"], &input);
    assert!(output.status.success(), "{output:?}");

    let replies = messages(&output);
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert_eq!(replies[0]["id"], 0);
    assert_eq!(replies[0]["result"]["protocolVersion"], 1);
    assert_eq!(replies[0]["result"]["agentInfo"]["name"], "agent86");
    // Not built yet: said plainly, not faked.
    assert_eq!(replies[1]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32601);
}

#[test]
fn it_ends_cleanly_when_the_client_closes_stdin_without_a_word() {
    let workspace = Workspace::new("silent");
    let output = workspace.run(&["acp"], "");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
}

#[test]
fn garbage_input_is_answered_and_the_connection_goes_on() {
    let workspace = Workspace::new("garbage");
    let input = format!("not json\n{INITIALIZE}\n");
    let output = workspace.run(&["acp"], &input);
    assert!(output.status.success(), "{output:?}");
    let replies = messages(&output);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["result"]["protocolVersion"], 1);
}

#[test]
fn a_broken_config_keeps_stdout_clean() {
    let workspace = Workspace::new("badconfig");
    std::fs::write(
        workspace
            .root
            .join("home")
            .join(".agent86")
            .join("config.toml"),
        "[model\n",
    )
    .unwrap();
    let output = workspace.run(&["acp"], &format!("{INITIALIZE}\n"));
    // The error is on stderr as for every command, and nothing reaches stdout.
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty());
}
