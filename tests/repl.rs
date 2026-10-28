//! End-to-end tests of the interactive REPL (`agent86` with no subcommand), driven
//! through piped stdin against the local mock provider.

mod support;

use regex::Regex;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use support::{fixture, sse_text, MockServer, Reply};

/// An isolated home and project directory for one test.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str, server: &MockServer) -> Self {
        let root = std::env::temp_dir().join(format!("agent86-repl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home").join(".agent86")).unwrap();
        std::fs::create_dir_all(root.join("project").join(".agent86")).unwrap();
        // The default sentence-transformers model is not cached in the throwaway
        // home; the hash embedder keeps the sessions self-contained.
        std::fs::write(
            root.join("home").join(".agent86").join("config.toml"),
            "[memory]\nembeddings = \"hash:256\"\n",
        )
        .unwrap();
        let workspace = Self { root };
        workspace.write_config(&format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_key_env = \"AGENT86_TEST_MOCK_KEY\"\nmax_retries = 0\n",
            server.base_url
        ));
        workspace
    }

    fn write_config(&self, text: &str) {
        std::fs::write(self.project().join(".agent86").join("config.toml"), text).unwrap();
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Run `agent86 <args>` with `input` piped to stdin, then end of input.
    async fn repl(&self, args: &[&str], input: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent86"));
        command
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("AGENT86_TEST_MOCK_KEY", "sk-test-0123456789abcdefghij")
            .env_remove("AGENT86_MODEL")
            .env_remove("AGENT86_SANDBOX")
            .env_remove("AGENT86_APPROVAL")
            .env_remove("NO_COLOR")
            // No test may reach the Hugging Face hub or read the real cache.
            .env_remove("HF_HUB_CACHE")
            .env_remove("HF_HOME")
            .env_remove("HF_ENDPOINT")
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let input = input.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// The session id from the `session <id>` startup note.
fn session_id(text: &str) -> String {
    Regex::new(r"(?m)^session ([0-9a-f]{12})$")
        .unwrap()
        .captures(text)
        .unwrap_or_else(|| panic!("no session note in {text:?}"))[1]
        .to_owned()
}

#[tokio::test]
async fn piped_input_runs_turns_and_commands_until_end_of_input() {
    let server = MockServer::start(vec![Reply::Sse(fixture("openai/text.sse"))]).await;
    let workspace = Workspace::new("basic", &server);

    let output = workspace
        .repl(
            &[],
            "Say hello\n/cost\n/mode\n/mode bogus\n\n/nope\n/skills\n",
        )
        .await;

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    let id = session_id(&text);
    // Durations vary; everything else is fixed.
    let text = Regex::new(r"· \d+\.\ds\n")
        .unwrap()
        .replace(&text, "· 0.0s\n");
    assert_eq!(
        text,
        format!(
            "session {id}\n\
             agent86> \n\
             agent86 Hello, world\n\
             — 1 step · 0 tools · 12 in / 3 out (4 cached) · cost n/a (unpriced model) · 0.0s\n\
             \n\
             agent86> steps 1  in 12  out 3 tok  cost n/a (unpriced model)\n\
             cache read 4  written 0 tok\n\
             agent86> approval mode: auto\n\
             agent86> unknown mode 'bogus' (ask|auto|deny)\n\
             agent86> agent86> unknown command /nope\n\
             agent86> no skills discovered\n\
             agent86> \n\
             bye\n"
        )
    );
    // No banner, no colour: stdout is not a terminal.
    assert!(!text.contains('╭') && !text.contains('\x1b'));
    let sent = &server.requests()[0];
    assert_eq!(
        sent.body["messages"][1],
        serde_json::json!({"role": "user", "content": "Say hello"})
    );
}

#[tokio::test]
async fn exit_and_quit_say_bye() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("exit", &server);

    for command in ["/exit\nignored\n", "/quit\n"] {
        let output = workspace.repl(&[], command).await;
        assert!(output.status.success());
        assert!(stdout(&output).ends_with("agent86> bye\n"), "{output:?}");
    }
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn mentions_inline_files_and_report_what_was_not_attached() {
    let server = MockServer::start(vec![sse_text("Read it.", "stop")]).await;
    let workspace = Workspace::new("mentions", &server);
    std::fs::write(workspace.project().join("notes.txt"), "remember the milk\n").unwrap();

    let output = workspace
        .repl(&[], "Summarize @notes.txt and @missing.txt\n")
        .await;

    let text = stdout(&output);
    assert!(
        text.contains("agent86> @missing.txt: no such file or directory\n\nagent86 Read it.\n"),
        "{text}"
    );
    let sent = &server.requests()[0];
    assert_eq!(
        sent.body["messages"][1]["content"],
        "Summarize @notes.txt and @missing.txt\n\n\
         --- @notes.txt (1 lines) ---\n```\nremember the milk\n```\n\n\
         --- @missing.txt (not attached) ---\nno such file or directory"
    );
    // The session is named after what the user typed, not the attached files.
    let listed = workspace.repl(&[], "/sessions\n").await;
    assert!(
        stdout(&listed).contains("Summarize @notes.txt and @missing.txt"),
        "{listed:?}"
    );
}

#[tokio::test]
async fn sessions_can_be_listed_and_resumed_by_id_or_prefix() {
    let server = MockServer::start(vec![
        Reply::Sse(fixture("openai/text.sse")),
        sse_text("You said hello.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("sessions", &server);

    let first = stdout(&workspace.repl(&[], "Say hello\n").await);
    let id = session_id(&first);

    let resumed = stdout(
        &workspace
            .repl(&["--resume", &id], "What did I say?\n")
            .await,
    );
    assert!(
        resumed.starts_with(&format!(
            "resumed session {id} (2 messages) - Say hello\nsession {id}\n"
        )),
        "{resumed}"
    );
    // The resumed history went to the model.
    let sent = &server.requests()[1];
    let messages = sent.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2]["content"], "Hello, world");

    let listing = stdout(
        &workspace
            .repl(&[], &format!("/sessions\n/resume {}\n/resume\n", &id[..8]))
            .await,
    );
    let rows = Regex::new(&format!(r"(?m)^{}  Say hello +just now$", &id[..8])).unwrap();
    assert!(
        listing.contains("Recent sessions\nSession   Title"),
        "{listing}"
    );
    assert!(rows.is_match(&listing), "{listing}");
    assert!(
        listing.contains(&format!(
            "agent86> resumed session {id} (4 messages) - Say hello\n"
        )),
        "{listing}"
    );

    let unknown = stdout(&workspace.repl(&["-r", "nope"], "").await);
    assert!(
        unknown.starts_with("no session 'nope' found; starting fresh\nsession "),
        "{unknown}"
    );
    let missing = stdout(&workspace.repl(&[], "/resume zzz\n").await);
    assert!(
        missing.contains("agent86> no session 'zzz' found\n"),
        "{missing}"
    );
}

#[tokio::test]
async fn clear_starts_a_new_session_and_memory_reports_it() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("clear", &server);

    let text = stdout(&workspace.repl(&[], "/memory\n/clear\n/memory\n").await);

    let id = session_id(&text);
    let memory =
        Regex::new(r"memory sessions (\d+)  episodes 0  facts 0  session ([0-9a-f]{12})").unwrap();
    let reports: Vec<(String, String)> = memory
        .captures_iter(&text)
        .map(|found| (found[1].to_owned(), found[2].to_owned()))
        .collect();
    assert_eq!(reports.len(), 2, "{text}");
    assert_eq!(reports[0], ("1".to_owned(), id.clone()));
    assert_eq!(reports[1].0, "2");
    assert_ne!(reports[1].1, id);
    assert!(text.contains("agent86> conversation cleared\n"), "{text}");
}

#[tokio::test]
async fn memory_off_means_nothing_to_resume() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("no-memory", &server);
    let config =
        std::fs::read_to_string(workspace.project().join(".agent86").join("config.toml")).unwrap();
    workspace.write_config(&format!("{config}\n[memory]\nenabled = false\n"));

    let text = stdout(&workspace.repl(&[], "/memory\n/sessions\n/resume x\n").await);

    assert!(text.contains("agent86> memory is disabled\n"), "{text}");
    let note = "memory is disabled - sessions are not saved, so there is nothing to resume";
    assert_eq!(text.matches(note).count(), 2, "{text}");
}

#[tokio::test]
async fn prompts_are_recorded_in_the_history_file_by_bash_rules() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("history", &server);

    workspace
        .repl(&[], "/cost\n/cost\n secret\n/mode auto\n")
        .await;

    let history =
        std::fs::read_to_string(workspace.home().join(".agent86").join("history")).unwrap();
    assert_eq!(history, "/cost\n/mode auto\n");
}

#[tokio::test]
async fn tools_model_and_config_commands_report_the_session() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("commands", &server);

    let text = stdout(
        &workspace
            .repl(
                &[],
                "/tools\n/model\n/model mock:other/model\n/model nope\n/config\n",
            )
            .await,
    );

    assert!(
        text.contains(
            "agent86> tools: edit_file, list_dir, python_exec, read_file, recall, remember, run_command, web_fetch, write_file\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "agent86> current model: openai:test/model\n\
             usage: /model <provider:model>  e.g. /model openrouter:anthropic/claude-3.7-sonnet\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("agent86> model: openai:other/model\n"),
        "{text}"
    );
    assert!(
        text.contains("agent86> Model reference 'nope' must be 'provider:model'"),
        "{text}"
    );
    assert!(text.contains("\"default\": \"mock:test/model\""), "{text}");
}

#[tokio::test]
async fn piped_turns_decline_side_effects() {
    let server = MockServer::start(vec![
        support::sse_tool_calls(&[("w", "write_file", r#"{"path": "x.txt", "content": "hi"}"#)]),
        sse_text("Could not write.", "stop"),
    ])
    .await;
    let workspace = Workspace::new("approval", &server);

    let text = stdout(&workspace.repl(&[], "write x.txt\n").await);

    assert!(
        text.contains(
            "[tool] write_file -> error: Not executed: approval required but no prompt available.\n"
        ),
        "{text}"
    );
    assert!(!workspace.project().join("x.txt").exists());
}

#[tokio::test]
async fn provider_errors_are_reported_and_the_loop_goes_on() {
    let server = MockServer::start(vec![Reply::Status {
        status: 400,
        headers: Vec::new(),
        body: "{\"error\": \"bad request\"}".to_owned(),
    }])
    .await;
    let workspace = Workspace::new("errors", &server);

    let output = workspace.repl(&[], "hello\n/cost\n").await;

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("agent86 \nerror: "), "{text}");
    assert!(text.contains("agent86> steps 0  in 0  out 0 tok"), "{text}");
    assert!(text.ends_with("agent86> \nbye\n"), "{text}");
}

#[tokio::test]
async fn an_unusable_model_cannot_start_the_repl() {
    let server = MockServer::start(vec![]).await;
    let workspace = Workspace::new("bad-model", &server);

    let output = workspace.repl(&["--model", "nope"], "").await;

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(
        text.starts_with("Cannot start: Model reference 'nope' must be 'provider:model'"),
        "{text}"
    );
    assert!(!text.contains("agent86>"));
}
