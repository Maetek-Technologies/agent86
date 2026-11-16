//! Remembered secrets in editor (ACP) sessions: a secret entered at a terminal and
//! remembered for the project is loaded into that project's sessions, the model
//! sees its name and never its value, and `/secret forget` removes it for good.
//! Since 1.0.23, a client with its own masked entry hands secrets over through the
//! `com.maetek.agent86/secrets` extension, and answers `request_secret`.
//! The keyring is an in-memory fake and the home directory a temporary one, so
//! nothing here touches the real keyring or the real `~/.agent86`.

mod support;

use agent86::acp::harness::HarnessBackend;
use agent86::acp::{
    Backend, ClientCapabilities, Permission, PermissionRequest, SecretMethod, SecretReply,
    TurnSink, Update, SECRET_REQUEST_METHOD,
};
use agent86::config::{load_config, ConfigPaths, Overrides, ProjectTrust};
use agent86::secrets::{SecretStore, SecretStoreError, SharedStore};
use agent86::tools::ToolContext;
use agent86::vault::SecretAnswer;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{sse_text, sse_tool_calls, MockServer};

const VALUE: &str = "tok_9f8e7d6c5b4a3921";

/// An in-memory keyring that outlives a session, as the real one does.
#[derive(Default)]
struct FakeKeyring {
    entries: Mutex<Vec<((String, String), String)>>,
}

impl SecretStore for FakeKeyring {
    fn get_password(&self, service: &str, account: &str) -> Option<String> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .find(|(key, _)| key.0 == service && key.1 == account)
            .map(|(_, value)| value.clone())
    }

    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError> {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
        entries.push(((service.to_owned(), account.to_owned()), secret.to_owned()));
        Ok(())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<bool, SecretStoreError> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|(key, _)| !(key.0 == service && key.1 == account));
        Ok(entries.len() != before)
    }
}

/// A turn's sink that keeps what the model said.
#[derive(Default)]
struct Said(Mutex<String>);

impl TurnSink for Said {
    fn update(&self, update: Update) {
        if let Update::Message(text) = update {
            self.0.lock().unwrap().push_str(&text);
        }
    }
    fn ask(&self, _: PermissionRequest) -> Permission {
        Permission::Declined
    }
    fn call(&self, _: &str, _: Value, _: Option<Duration>) -> Result<Value, String> {
        Err("no editor".into())
    }
}

struct Dirs {
    root: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-acp-secrets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { root }
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn editor_sessions_load_what_was_remembered_for_the_project_and_can_forget_it() {
    let dirs = Dirs::new("remembered");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let keyring = Arc::new(FakeKeyring::default());

    // At a terminal, in this project, the user entered a secret and remembered it.
    let terminal = ToolContext::new(&project).unwrap();
    terminal
        .secrets
        .attach_store(keyring.clone(), terminal.policy.workspace());
    terminal
        .secrets
        .accept(
            "API_TOKEN",
            SecretAnswer {
                value: VALUE.to_owned(),
                remember: true,
            },
        )
        .unwrap();

    let server = MockServer::start(vec![sse_text("Hello.", "stop")]).await;
    let config_file = dirs.root.join("config.toml");
    std::fs::write(
        &config_file,
        format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\nmax_retries = 0\n\n[memory]\nenabled = false\n\n[observability]\ntrace = false\n",
            server.base_url
        ),
    )
    .unwrap();
    let config = load_config(
        &ConfigPaths {
            user: None,
            project: config_file,
            project_trust: ProjectTrust::Trusted,
        },
        &Overrides::default(),
    )
    .unwrap();
    let store: SharedStore = keyring.clone();
    let home = dirs.root.join("home");

    // The backend runs its own runtime, so it works off the test's.
    let outcome = tokio::task::spawn_blocking({
        let project = project.clone();
        move || {
            let mut backend = HarnessBackend::with_secret_store(config, Some(store))
                .unwrap()
                .with_home(Some(home));
            let session = backend.new_session(&project, Vec::new()).unwrap();
            let listing = backend.run_command(&session, "/secret").unwrap();
            // Something typed after the command is neither stored nor repeated.
            let typed = backend
                .run_command(&session, "/secret hunter2hunter2hunter2")
                .unwrap();
            let sink: Arc<dyn TurnSink> = Arc::new(Said::default());
            backend.prompt(&session, "Hi", sink).unwrap();
            let bad = backend
                .run_command(&session, "/secret forget not-a-name!")
                .unwrap();
            let forgot = backend
                .run_command(&session, "/secret forget API_TOKEN")
                .unwrap();
            let after = backend.run_command(&session, "/secret").unwrap();
            // A new session in the same project: gone from the keyring, not just here.
            let next = backend.new_session(&project, Vec::new()).unwrap();
            let next_listing = backend.run_command(&next, "/secret").unwrap();
            (listing, typed, bad, forgot, after, next_listing)
        }
    })
    .await
    .unwrap();
    let (listing, typed, bad, forgot, after, next_listing) = outcome;

    assert!(listing.contains("API_TOKEN (remembered)"), "{listing}");
    assert!(!listing.contains(VALUE));
    assert!(typed.starts_with("nothing was stored."), "{typed}");
    assert!(!typed.contains("hunter2"), "{typed}");
    assert!(bad.contains("not a valid secret name"), "{bad}");
    assert_eq!(forgot, "secret API_TOKEN forgotten");
    assert!(after.starts_with("no secrets for this project"), "{after}");
    assert!(
        next_listing.starts_with("no secrets for this project"),
        "{next_listing}"
    );

    // The model was told the name, and nothing it received held the value.
    let requests = server.requests();
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("\n- API_TOKEN"), "{system}");
    for request in &requests {
        assert!(!request.body.to_string().contains(VALUE));
        assert!(!request.body.to_string().contains("hunter2"));
    }
    // The fake was the only keyring there was.
    assert!(keyring
        .entries
        .lock()
        .unwrap()
        .iter()
        .all(|(_, v)| v != VALUE));
}

#[tokio::test]
async fn without_a_keyring_there_is_nothing_to_load_and_the_command_says_how() {
    let dirs = Dirs::new("none");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let server = MockServer::start(vec![]).await;
    let config_file = dirs.root.join("config.toml");
    std::fs::write(
        &config_file,
        format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\n\n[memory]\nenabled = false\n\n[observability]\ntrace = false\n",
            server.base_url
        ),
    )
    .unwrap();
    let config = load_config(
        &ConfigPaths {
            user: None,
            project: config_file,
            project_trust: ProjectTrust::Trusted,
        },
        &Overrides::default(),
    )
    .unwrap();
    let home = dirs.root.join("home");
    let listing = tokio::task::spawn_blocking(move || {
        let mut backend = HarnessBackend::new(config).unwrap().with_home(Some(home));
        let session = backend.new_session(&project, Vec::new()).unwrap();
        backend.run_command(&session, "/secret").unwrap()
    })
    .await
    .unwrap();
    assert!(listing.contains("no secrets for this project"), "{listing}");
    assert!(listing.contains("Enter it at a terminal"), "{listing}");
}

/// A turn's sink that keeps every update and every question, and allows them all.
#[derive(Default)]
struct Recorded {
    updates: Mutex<Vec<Update>>,
    asked: Mutex<Vec<PermissionRequest>>,
}

impl TurnSink for Recorded {
    fn update(&self, update: Update) {
        self.updates.lock().unwrap().push(update);
    }
    fn ask(&self, request: PermissionRequest) -> Permission {
        self.asked.lock().unwrap().push(request);
        Permission::Allowed
    }
    fn call(&self, _: &str, _: Value, _: Option<Duration>) -> Result<Value, String> {
        Err("no editor".into())
    }
}

#[tokio::test]
async fn an_expanded_secret_comes_back_to_the_editor_as_its_placeholder_in_every_diff() {
    let dirs = Dirs::new("diff");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let keyring = Arc::new(FakeKeyring::default());
    let terminal = ToolContext::new(&project).unwrap();
    terminal
        .secrets
        .attach_store(keyring.clone(), terminal.policy.workspace());
    terminal
        .secrets
        .accept(
            "API_TOKEN",
            SecretAnswer {
                value: VALUE.to_owned(),
                remember: true,
            },
        )
        .unwrap();

    // The model writes the placeholder into `.env`, then edits the line holding it,
    // so the edit's old text read back from the disk holds the value.
    let server = MockServer::start(vec![
        sse_tool_calls(&[(
            "c1",
            "write_file",
            r#"{"path":".env","content":"TOKEN=${secret:API_TOKEN}\n"}"#,
        )]),
        sse_tool_calls(&[(
            "c2",
            "edit_file",
            r#"{"path":".env","old_string":"TOKEN=","new_string":"API_TOKEN="}"#,
        )]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let config_file = dirs.root.join("config.toml");
    std::fs::write(
        &config_file,
        format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\nmax_retries = 0\n\n[memory]\nenabled = false\n\n[observability]\ntrace = false\n\n[ui]\ntool_use = \"full\"\n",
            server.base_url
        ),
    )
    .unwrap();
    let config = load_config(
        &ConfigPaths {
            user: None,
            project: config_file,
            project_trust: ProjectTrust::Trusted,
        },
        &Overrides::default(),
    )
    .unwrap();
    let store: SharedStore = keyring.clone();
    let home = dirs.root.join("home");
    let sink = Arc::new(Recorded::default());
    tokio::task::spawn_blocking({
        let project = project.clone();
        let sink = Arc::clone(&sink);
        move || {
            let mut backend = HarnessBackend::with_secret_store(config, Some(store))
                .unwrap()
                .with_home(Some(home));
            let session = backend.new_session(&project, Vec::new()).unwrap();
            let sink: Arc<dyn TurnSink> = sink;
            backend
                .prompt(&session, "Set up the env file", sink)
                .unwrap();
        }
    })
    .await
    .unwrap();

    // The file holds the value: the expansion really happened.
    let written = std::fs::read_to_string(project.join(".env"))
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(written, format!("API_TOKEN={VALUE}\n"));

    let placeholder = "${secret:API_TOKEN}";
    let path = agent86::tools::display_path(&project.join(".env"));
    let asked = sink.asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{asked:?}");
    let write = asked[0].diff.as_ref().expect("the write's diff");
    assert_eq!(write.path, path);
    assert_eq!(write.old_text, None);
    assert_eq!(write.new_text, format!("TOKEN={placeholder}\n"));
    let edit = asked[1].diff.as_ref().expect("the edit's diff");
    assert_eq!(
        edit.old_text.as_deref(),
        Some(format!("TOKEN={placeholder}\n").as_str())
    );
    assert_eq!(edit.new_text, format!("API_TOKEN={placeholder}\n"));

    let updates = sink.updates.lock().unwrap();
    let diffs: Vec<&agent86::acp::FileChange> = updates
        .iter()
        .filter_map(|update| match update {
            Update::ToolResult { diff, .. } => diff.as_ref(),
            _ => None,
        })
        .collect();
    assert_eq!(diffs.len(), 2, "{updates:?}");
    assert_eq!(diffs[0].new_text, format!("TOKEN={placeholder}\n"));
    assert_eq!(
        diffs[1].old_text.as_deref(),
        Some(format!("TOKEN={placeholder}\n").as_str())
    );
    assert_eq!(diffs[1].new_text, format!("API_TOKEN={placeholder}\n"));

    // Nothing the editor was sent held the value, anywhere.
    let everything = format!("{asked:?}{updates:?}");
    assert!(!everything.contains(VALUE), "{everything}");
    for request in server.requests() {
        assert!(!request.body.to_string().contains(VALUE));
    }
}

/// The config for a session on the mock provider, with memory and the trace off
/// and every call allowed.
fn mock_config(dirs: &Dirs, base_url: &str) -> agent86::config::Config {
    let config_file = dirs.root.join("config.toml");
    std::fs::write(
        &config_file,
        format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{base_url}\"\nmax_retries = 0\n\n[memory]\nenabled = false\n\n[observability]\ntrace = false\n\n[guardrails]\napproval = \"auto\"\n"
        ),
    )
    .unwrap();
    load_config(
        &ConfigPaths {
            user: None,
            project: config_file,
            project_trust: ProjectTrust::Trusted,
        },
        &Overrides::default(),
    )
    .unwrap()
}

fn set(name: &str, value: &str, remember: bool) -> SecretMethod {
    SecretMethod::Set {
        name: name.to_owned(),
        answer: SecretAnswer {
            value: value.to_owned(),
            remember,
        },
    }
}

fn forget(name: &str) -> SecretMethod {
    SecretMethod::Forget {
        name: name.to_owned(),
    }
}

fn listing(secrets: &[(&str, bool)], can_remember: bool) -> SecretReply {
    SecretReply::List {
        secrets: secrets
            .iter()
            .map(|(name, kept)| ((*name).to_owned(), *kept))
            .collect(),
        can_remember,
    }
}

/// The names of the tools a request to the model offered.
fn offered_tools(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// What `a_client_sets_lists_and_forgets_secrets...` collects inside the backend.
struct ClientRun {
    remembered: SecretReply,
    once: SecretReply,
    short: agent86::acp::BackendError,
    unknown: agent86::acp::BackendError,
    listed: SecretReply,
    next_listed: SecretReply,
    third_listed: SecretReply,
    forgot: SecretReply,
    again: SecretReply,
    fourth_listed: SecretReply,
    hint: String,
}

#[tokio::test]
async fn a_client_sets_lists_and_forgets_secrets_and_the_model_sees_only_names() {
    let dirs = Dirs::new("client-set");
    let project = dirs.root.join("project").canonicalize().unwrap();
    // The model reads a file that holds the value; the result must come back as
    // the placeholder.
    std::fs::write(project.join("notes.txt"), format!("token: {VALUE}\n")).unwrap();
    let server = MockServer::start(vec![
        sse_tool_calls(&[("c1", "read_file", r#"{"path":"notes.txt"}"#)]),
        sse_text("Read it.", "stop"),
    ])
    .await;
    let config = mock_config(&dirs, &server.base_url);
    let keyring = Arc::new(FakeKeyring::default());
    let store: SharedStore = keyring.clone();
    let home = dirs.root.join("home");
    let sink = Arc::new(Recorded::default());
    let run = tokio::task::spawn_blocking({
        let project = project.clone();
        let sink = Arc::clone(&sink);
        move || {
            let mut backend = HarnessBackend::with_secret_store(config, Some(store))
                .unwrap()
                .with_home(Some(home));
            let session = backend.new_session(&project, Vec::new()).unwrap();
            let remembered = backend
                .secrets(&session, set("API_TOKEN", VALUE, true))
                .unwrap();
            let once = backend
                .secrets(&session, set("SCRATCH_KEY", "scratch-value-1", false))
                .unwrap();
            let short = backend
                .secrets(&session, set("TINY", "abc", false))
                .unwrap_err();
            let unknown = backend.secrets("nope", SecretMethod::List).unwrap_err();
            let listed = backend.secrets(&session, SecretMethod::List).unwrap();
            let sink: Arc<dyn TurnSink> = sink;
            backend.prompt(&session, "Read notes.txt", sink).unwrap();
            // Another session in the project loads only what was remembered.
            let next = backend.new_session(&project, Vec::new()).unwrap();
            let next_listed = backend.secrets(&next, SecretMethod::List).unwrap();
            // "Just this once" on a remembered name takes it out of the keyring.
            backend
                .secrets(&next, set("API_TOKEN", VALUE, false))
                .unwrap();
            let third = backend.new_session(&project, Vec::new()).unwrap();
            let third_listed = backend.secrets(&third, SecretMethod::List).unwrap();
            backend
                .secrets(&third, set("API_TOKEN", VALUE, true))
                .unwrap();
            let forgot = backend.secrets(&third, forget("API_TOKEN")).unwrap();
            let again = backend.secrets(&third, forget("API_TOKEN")).unwrap();
            let fourth = backend.new_session(&project, Vec::new()).unwrap();
            let fourth_listed = backend.secrets(&fourth, SecretMethod::List).unwrap();
            let hint = backend.run_command(&fourth, "/secret").unwrap();
            ClientRun {
                remembered,
                once,
                short,
                unknown,
                listed,
                next_listed,
                third_listed,
                forgot,
                again,
                fourth_listed,
                hint,
            }
        }
    })
    .await
    .unwrap();

    let stored = |remembered: bool| {
        SecretReply::Set(agent86::vault::Stored {
            remembered,
            warning: None,
        })
    };
    assert_eq!(run.remembered, stored(true));
    assert_eq!(run.once, stored(false));
    assert_eq!(run.short.code, agent86::acp::INVALID_PARAMS);
    assert!(!run.short.message.contains("abc"), "{}", run.short.message);
    assert_eq!(run.unknown.code, agent86::acp::RESOURCE_NOT_FOUND);
    assert_eq!(
        run.listed,
        listing(&[("API_TOKEN", true), ("SCRATCH_KEY", false)], true)
    );
    assert_eq!(run.next_listed, listing(&[("API_TOKEN", true)], true));
    assert_eq!(run.third_listed, listing(&[], true));
    let forgotten = |forgotten: bool| SecretReply::Forgotten {
        forgotten,
        warning: None,
    };
    assert_eq!(run.forgot, forgotten(true));
    assert_eq!(run.again, forgotten(false));
    assert_eq!(run.fourth_listed, listing(&[], true));
    // A client without its own entry is still pointed at the terminal.
    assert!(run.hint.contains("Enter it at a terminal"), "{}", run.hint);
    assert!(keyring.entries.lock().unwrap().is_empty());

    // The model was told the names, and nothing it or the editor got held a value.
    let requests = server.requests();
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("\n- API_TOKEN"), "{system}");
    assert!(system.contains("\n- SCRATCH_KEY"), "{system}");
    assert!(!offered_tools(&requests[0].body).contains(&"request_secret".to_owned()));
    let result = requests[1].body.to_string();
    assert!(result.contains("token: ${secret:API_TOKEN}"), "{result}");
    for request in &requests {
        assert!(!request.body.to_string().contains(VALUE));
    }
    let updates = format!("{:?}", sink.updates.lock().unwrap());
    assert!(updates.contains("${secret:API_TOKEN}"), "{updates}");
    assert!(!updates.contains(VALUE), "{updates}");
}

#[tokio::test]
async fn without_a_keyring_a_secret_to_remember_is_kept_for_the_session_and_says_so() {
    let dirs = Dirs::new("client-nokeyring");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let server = MockServer::start(vec![]).await;
    let config = mock_config(&dirs, &server.base_url);
    let home = dirs.root.join("home");
    let (reply, listed) = tokio::task::spawn_blocking(move || {
        let mut backend = HarnessBackend::new(config).unwrap().with_home(Some(home));
        let session = backend.new_session(&project, Vec::new()).unwrap();
        let reply = backend
            .secrets(&session, set("API_TOKEN", VALUE, true))
            .unwrap();
        let listed = backend.secrets(&session, SecretMethod::List).unwrap();
        (reply, listed)
    })
    .await
    .unwrap();
    let SecretReply::Set(stored) = reply else {
        panic!("{reply:?}");
    };
    assert!(!stored.remembered);
    let warning = stored.warning.unwrap_or_default();
    assert!(warning.contains("no keyring"), "{warning}");
    assert_eq!(listed, listing(&[("API_TOKEN", false)], false));
}

/// A client with its own masked entry: it declines the first `request_secret`, then
/// enters the value (and asks to remember it) for the next, and allows every call.
#[derive(Default)]
struct MaskedEntry {
    updates: Mutex<Vec<Update>>,
    asked: Mutex<Vec<Value>>,
}

impl TurnSink for MaskedEntry {
    fn update(&self, update: Update) {
        self.updates.lock().unwrap().push(update);
    }
    fn ask(&self, _: PermissionRequest) -> Permission {
        Permission::Allowed
    }
    fn call(&self, method: &str, params: Value, _: Option<Duration>) -> Result<Value, String> {
        assert_eq!(method, SECRET_REQUEST_METHOD);
        let mut asked = self.asked.lock().unwrap();
        asked.push(params);
        if asked.len() == 1 {
            return Ok(serde_json::json!({ "outcome": "declined" }));
        }
        Ok(serde_json::json!({ "outcome": "entered", "value": VALUE, "remember": true }))
    }
}

#[tokio::test]
async fn request_secret_is_answered_by_a_client_with_its_own_masked_entry() {
    let dirs = Dirs::new("client-request");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let ask = r#"{"name":"API_TOKEN","reason":"to deploy"}"#;
    let server = MockServer::start(vec![
        sse_tool_calls(&[("c1", "request_secret", ask)]),
        sse_tool_calls(&[("c2", "request_secret", ask)]),
        sse_tool_calls(&[(
            "c3",
            "write_file",
            r#"{"path":".env","content":"TOKEN=${secret:API_TOKEN}\n"}"#,
        )]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let config = mock_config(&dirs, &server.base_url);
    let keyring = Arc::new(FakeKeyring::default());
    let store: SharedStore = keyring.clone();
    let home = dirs.root.join("home");
    let sink = Arc::new(MaskedEntry::default());
    let (session, hint, next_listed) = tokio::task::spawn_blocking({
        let project = project.clone();
        let sink = Arc::clone(&sink);
        move || {
            let mut backend = HarnessBackend::with_secret_store(config, Some(store))
                .unwrap()
                .with_home(Some(home));
            backend.client_capabilities(ClientCapabilities {
                secrets: true,
                ..ClientCapabilities::default()
            });
            let session = backend.new_session(&project, Vec::new()).unwrap();
            let hint = backend.run_command(&session, "/secret").unwrap();
            let sink: Arc<dyn TurnSink> = sink;
            backend
                .prompt(&session, "Set up the env file", sink)
                .unwrap();
            let next = backend.new_session(&project, Vec::new()).unwrap();
            let next_listed = backend.secrets(&next, SecretMethod::List).unwrap();
            (session, hint, next_listed)
        }
    })
    .await
    .unwrap();

    assert!(hint.contains("this client's own secret entry"), "{hint}");
    // The client was asked twice, in the extension's shape.
    let asked = sink.asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(
        asked[0],
        serde_json::json!({
            "sessionId": session,
            "name": "API_TOKEN",
            "reason": "to deploy",
            "replacing": false,
            "canRemember": true,
            "remembered": false,
        })
    );
    assert_eq!(asked[1]["name"], "API_TOKEN");
    // The first was declined, and the model was told so; the second was set.
    let requests = server.requests();
    assert!(offered_tools(&requests[0].body).contains(&"request_secret".to_owned()));
    let second = requests[1].body.to_string();
    assert!(
        second.contains("The user declined to enter the secret API_TOKEN"),
        "{second}"
    );
    let third = requests[2].body.to_string();
    assert!(third.contains("The secret API_TOKEN is set"), "{third}");
    assert!(third.contains("remember it for this project"), "{third}");
    // The value was used, and remembered for the project.
    let written = std::fs::read_to_string(project.join(".env"))
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(written, format!("TOKEN={VALUE}\n"));
    assert_eq!(next_listed, listing(&[("API_TOKEN", true)], true));
    // Nothing the model or the editor was sent held the value.
    for request in &requests {
        assert!(!request.body.to_string().contains(VALUE));
    }
    let updates = format!("{:?}", sink.updates.lock().unwrap());
    assert!(updates.contains("${secret:API_TOKEN}"), "{updates}");
    assert!(!updates.contains(VALUE), "{updates}");
}

#[tokio::test]
async fn a_reason_holding_a_known_value_reaches_the_client_as_its_placeholder() {
    let dirs = Dirs::new("client-reason");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let ask = format!(r#"{{"name":"NEW_TOKEN","reason":"to replace {VALUE}"}}"#);
    let server = MockServer::start(vec![
        sse_tool_calls(&[("c1", "request_secret", &ask)]),
        sse_text("Done.", "stop"),
    ])
    .await;
    let config = mock_config(&dirs, &server.base_url);
    let home = dirs.root.join("home");
    let sink = Arc::new(MaskedEntry::default());
    tokio::task::spawn_blocking({
        let sink = Arc::clone(&sink);
        move || {
            let mut backend = HarnessBackend::new(config).unwrap().with_home(Some(home));
            backend.client_capabilities(ClientCapabilities {
                secrets: true,
                ..ClientCapabilities::default()
            });
            let session = backend.new_session(&project, Vec::new()).unwrap();
            backend
                .secrets(&session, set("OLD_TOKEN", VALUE, false))
                .unwrap();
            let sink: Arc<dyn TurnSink> = sink;
            backend.prompt(&session, "Rotate the token", sink).unwrap();
        }
    })
    .await
    .unwrap();
    let asked = sink.asked.lock().unwrap();
    assert_eq!(asked[0]["reason"], "to replace ${secret:OLD_TOKEN}");
    // No keyring, so the client is told it cannot remember one.
    assert_eq!(asked[0]["canRemember"], false);
}

#[tokio::test]
async fn request_secret_is_not_offered_to_a_client_without_its_own_entry() {
    let dirs = Dirs::new("client-plain");
    let project = dirs.root.join("project").canonicalize().unwrap();
    let server = MockServer::start(vec![sse_text("Hi.", "stop")]).await;
    let config = mock_config(&dirs, &server.base_url);
    let home = dirs.root.join("home");
    tokio::task::spawn_blocking(move || {
        let mut backend = HarnessBackend::new(config).unwrap().with_home(Some(home));
        backend.client_capabilities(ClientCapabilities {
            state: true,
            ..ClientCapabilities::default()
        });
        let session = backend.new_session(&project, Vec::new()).unwrap();
        let sink: Arc<dyn TurnSink> = Arc::new(Said::default());
        backend.prompt(&session, "Hi", sink).unwrap();
    })
    .await
    .unwrap();
    let requests = server.requests();
    let tools = offered_tools(&requests[0].body);
    assert!(tools.contains(&"read_file".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"request_secret".to_owned()), "{tools:?}");
}
