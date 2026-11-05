//! Remembered secrets in editor (ACP) sessions: a secret entered at a terminal and
//! remembered for the project is loaded into that project's sessions, the model
//! sees its name and never its value, and `/secret forget` removes it for good.
//! The keyring is an in-memory fake and the home directory a temporary one, so
//! nothing here touches the real keyring or the real `~/.agent86`.

mod support;

use agent86::acp::harness::HarnessBackend;
use agent86::acp::{Backend, Permission, PermissionRequest, TurnSink, Update};
use agent86::config::{load_config, ConfigPaths, Overrides, ProjectTrust};
use agent86::secrets::{SecretStore, SecretStoreError, SharedStore};
use agent86::tools::ToolContext;
use agent86::vault::SecretAnswer;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{sse_text, MockServer};

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
