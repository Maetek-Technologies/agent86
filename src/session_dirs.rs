//! The directory each session works in, beyond Python.
//!
//! The session database has no working-directory column, and its schema is shared
//! with the Python implementation, so the directory is kept beside it, in
//! `~/.agent86/acp-sessions.json`: one JSON object mapping a session id to the
//! absolute directory it last worked in. The file is named for the ACP server,
//! which was its first writer and is what reads it (`session/list`); since 1.0.17
//! the terminal (the interface, the plain loop, and `run`) records its sessions
//! there too, so an editor's or a browser's list filtered by directory shows them
//! under the right project and no other.
//!
//! Nothing in a saved session's state records where it ran (the system prompt is
//! rebuilt for each request and names no directory, and steps carry none), so a
//! session saved before its directory was recorded stays unknown until it is
//! resumed. It is never guessed from the paths its tool calls touched.
//!
//! Several agent86 processes can write the file at once (editor windows, terminals,
//! a browser's listing processes). Each update is a read-modify-write done under an
//! exclusive lock on a lock file beside it (`acp-sessions.json.lock`, which is left
//! in place: deleting a lock file reopens the race it closes), and the new text is
//! written to a temporary file and renamed over the old, so a reader, which takes
//! no lock, sees either the old map or the new one. If the lock cannot be had (a
//! file system without locks, or a holder stuck for longer than [`LOCK_WAIT`]), the
//! update goes ahead unlocked, as before 1.0.17: the worst case is one lost entry,
//! which only means that session is unknown until it is next recorded.

use crate::session::SessionStore;
use serde_json::{Map, Value};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The file's name, under `~/.agent86`.
pub const FILE_NAME: &str = "acp-sessions.json";

/// How long an update waits for another process's update to finish.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The record of where each session works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDirs {
    file: PathBuf,
}

impl SessionDirs {
    /// The record under `home` (`<home>/.agent86/acp-sessions.json`). Tests pass a
    /// temporary directory, so they never touch the user's.
    pub fn under_home(home: &Path) -> Self {
        Self {
            file: home.join(".agent86").join(FILE_NAME),
        }
    }

    /// The user's record, or `None` when there is no home directory.
    pub fn for_user() -> Option<Self> {
        crate::config::home_dir().map(|home| Self::under_home(&home))
    }

    /// The file itself.
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Every recorded directory, by session id. A missing or unreadable file is an
    /// empty record.
    pub fn read(&self) -> Map<String, Value> {
        read_map(&self.file)
    }

    /// The directory recorded for `session_id`, as it was written.
    pub fn get(&self, session_id: &str) -> Option<String> {
        self.read()
            .get(session_id)
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Record that `session_id` works in `cwd`, replacing any earlier directory.
    /// Best effort: a failure to write is not reported, since the session itself
    /// is saved either way.
    pub fn record(&self, session_id: &str, cwd: &Path) {
        let dir = crate::tools::display_path(cwd);
        let Some(parent) = self.file.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        // Held until the end of this function; dropping the file releases the lock.
        let _lock = self.lock();
        let mut known = read_map(&self.file);
        if known.get(session_id).and_then(Value::as_str) == Some(dir.as_str()) {
            return;
        }
        known.insert(session_id.to_owned(), Value::String(dir));
        let Ok(text) = serde_json::to_string(&Value::Object(known)) else {
            return;
        };
        // Per process, so two writers that both went ahead unlocked cannot write
        // into one temporary file.
        let staged = self
            .file
            .with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&staged, text).is_err() || std::fs::rename(&staged, &self.file).is_err() {
            let _ = std::fs::remove_file(&staged);
        }
    }

    /// Record `session_id`'s directory once the session has a title, which is when
    /// a session list starts showing it. Returns whether it is recorded.
    pub fn record_if_titled(&self, store: &SessionStore, session_id: &str, cwd: &Path) -> bool {
        if !matches!(store.title(session_id), Ok(Some(_))) {
            return false;
        }
        self.record(session_id, cwd);
        true
    }

    /// The exclusive lock on the lock file, waiting up to [`LOCK_WAIT`] for it, or
    /// `None` when it cannot be had (the update then goes ahead without it).
    fn lock(&self) -> Option<File> {
        let path = self.file.with_extension("json.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(file),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }
}

fn read_map(file: &Path) -> Map<String, Value> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// A directory as it is compared: slashes alike and no trailing one, and on Windows
/// without regard to case.
pub fn same_path_key(path: &str) -> String {
    let mut key = if cfg!(windows) {
        path.replace('\\', "/").to_lowercase()
    } else {
        path.to_owned()
    };
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn temp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("agent86-dirs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn a_recorded_directory_reads_back_and_replaces_the_last() {
        let home = temp_home("record");
        let dirs = SessionDirs::under_home(&home);
        assert_eq!(dirs.get("s1"), None);
        dirs.record("s1", Path::new("/work/one"));
        dirs.record("s2", Path::new("/work/two"));
        dirs.record("s1", Path::new("/work/three"));
        assert_eq!(dirs.path(), home.join(".agent86").join("acp-sessions.json"));
        assert_eq!(dirs.get("s1").as_deref(), Some("/work/three"));
        assert_eq!(dirs.get("s2").as_deref(), Some("/work/two"));
        // The format is the one earlier builds wrote and read: a flat object.
        let text = std::fs::read_to_string(dirs.path()).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"s1": "/work/three", "s2": "/work/two"})
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn writers_racing_each_other_lose_no_entry() {
        let home = temp_home("race");
        let dirs = SessionDirs::under_home(&home);
        let count = 16;
        let start = Arc::new(Barrier::new(count));
        std::thread::scope(|scope| {
            for n in 0..count {
                let dirs = dirs.clone();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    dirs.record(&format!("s{n}"), Path::new(&format!("/work/{n}")));
                });
            }
        });
        let known = dirs.read();
        assert_eq!(known.len(), count, "{known:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_session_is_recorded_once_it_has_a_title() {
        let home = temp_home("titled");
        let dirs = SessionDirs::under_home(&home);
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        let mut state = crate::session::AgentState::new();
        store.persist(&state).unwrap();
        assert!(!dirs.record_if_titled(&store, &state.session_id, Path::new("/w")));
        assert_eq!(dirs.get(&state.session_id), None);
        state.messages.push(crate::types::Message {
            role: crate::types::Role::User,
            content: "hi".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        });
        store.persist(&state).unwrap();
        assert!(dirs.record_if_titled(&store, &state.session_id, Path::new("/w")));
        assert_eq!(dirs.get(&state.session_id).as_deref(), Some("/w"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn paths_compare_without_trailing_slashes() {
        assert_eq!(same_path_key("/a/b/"), same_path_key("/a/b"));
        assert_ne!(same_path_key("/a/b"), same_path_key("/a/c"));
        if cfg!(windows) {
            assert_eq!(same_path_key(r"C:\Work\X"), same_path_key("c:/work/x/"));
        }
    }
}
