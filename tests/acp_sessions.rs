//! `session/list` over the harness backend, in process (1.0.17): each session is
//! listed under the directory recorded for it, wherever that was recorded (an
//! editor session, or a terminal one through the same record), and a session whose
//! directory was never recorded is left out of a filtered list rather than shown
//! under the directory the listing process started in. `/rename` changes the title
//! the list shows. The home directory is a temporary one (`with_home`), so nothing
//! here touches the real `~/.agent86`.

mod support;

use agent86::acp::harness::HarnessBackend;
use agent86::acp::Backend;
use agent86::config::{load_config, ConfigPaths, Overrides, ProjectTrust};
use agent86::session::{AgentState, SessionStore};
use agent86::session_dirs::SessionDirs;
use agent86::types::{Message, Role};
use std::path::{Path, PathBuf};
use support::MockServer;

struct Dirs {
    root: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agent86-acp-sessions-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["project", "terminal", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { root }
    }

    fn plain(&self, dir: &str) -> PathBuf {
        let path = self.root.join(dir).canonicalize().unwrap();
        let text = path.display().to_string();
        match text.strip_prefix(r"\\?\") {
            Some(plain) => PathBuf::from(plain),
            None => path,
        }
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A session saved as the terminal saves one, named after `question`.
fn saved_session(store: &SessionStore, question: &str) -> String {
    let mut state = AgentState::new();
    state.messages.push(Message {
        role: Role::User,
        content: question.to_owned(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    });
    store.persist(&state).unwrap();
    state.session_id
}

fn ids(list: &[agent86::acp::SessionSummary]) -> Vec<&str> {
    list.iter().map(|s| s.session_id.as_str()).collect()
}

#[tokio::test]
async fn sessions_are_listed_under_their_recorded_directory_and_unknown_ones_only_unfiltered() {
    let dirs = Dirs::new("list");
    let server = MockServer::start(vec![]).await;
    let database = dirs.root.join("memory.db");
    let config_file = dirs.root.join("config.toml");
    std::fs::write(
        &config_file,
        format!(
            "[model]\ndefault = \"mock:test/model\"\n\n[providers.mock]\nbase_url = \"{}\"\nmax_retries = 0\n\n[memory]\npath = '{}'\nembeddings = \"hash:256\"\n\n[observability]\ntrace = false\n",
            server.base_url,
            database.display()
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
    let (project, terminal) = (dirs.plain("project"), dirs.plain("terminal"));

    // A terminal session in `terminal`, recorded as the terminal records it, and one
    // saved before directories were recorded.
    let store = SessionStore::open(&database).unwrap();
    let from_terminal = saved_session(&store, "Terminal question");
    SessionDirs::under_home(&home).record(&from_terminal, &terminal);
    let unknown = saved_session(&store, "Question from an older build");
    drop(store);

    let startup = std::env::current_dir().unwrap();
    let (editor, here, there, at_startup, all, renamed) = tokio::task::spawn_blocking({
        let (project, terminal, startup) = (project.clone(), terminal.clone(), startup.clone());
        move || {
            let mut backend = HarnessBackend::new(config)
                .unwrap()
                .with_home(Some(home.clone()));
            // An editor session in the project, given its name with `/rename` (it has
            // no message, so it would not be listed otherwise).
            let editor = backend.new_session(&project, Vec::new()).unwrap();
            assert_eq!(backend.take_title_change(&editor), None);
            let reply = backend
                .run_command(&editor, "/rename  Editor thread ")
                .unwrap();
            assert_eq!(reply, "session renamed: Editor thread");
            let renamed = backend.take_title_change(&editor);
            assert_eq!(backend.take_title_change(&editor), None, "taken once");
            (
                editor,
                backend.list_sessions(Some(&project)),
                backend.list_sessions(Some(&terminal)),
                backend.list_sessions(Some(&startup)),
                backend.list_sessions(None),
                renamed,
            )
        }
    })
    .await
    .unwrap();

    assert_eq!(renamed.as_deref(), Some("Editor thread"));
    assert_eq!(ids(&here), [editor.as_str()]);
    assert_eq!(here[0].title.as_deref(), Some("Editor thread"));
    assert!(here[0].cwd_known);
    assert_eq!(ids(&there), [from_terminal.as_str()]);
    assert!(there[0].cwd_known);
    // The listing process's own directory no longer collects unknown sessions.
    assert!(
        !ids(&at_startup).contains(&unknown.as_str()),
        "{at_startup:?}"
    );

    // Unfiltered, every titled session is there; the unknown one carries the
    // stand-in directory, marked as such.
    assert_eq!(all.len(), 3, "{all:?}");
    let entry = |id: &str| all.iter().find(|s| s.session_id == id).unwrap();
    assert!(!entry(&unknown).cwd_known);
    assert_eq!(
        Path::new(&entry(&unknown).cwd),
        Path::new(&agent86::tools::display_path(&startup))
    );
    assert!(entry(&from_terminal).cwd_known);
    assert_eq!(Path::new(&entry(&from_terminal).cwd), terminal.as_path());
    assert!(entry(&editor).cwd_known);
    assert_eq!(Path::new(&entry(&editor).cwd), project.as_path());
}
