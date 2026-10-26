//! Prompt history, ported from Python's `ui/history.py`: the one file both
//! interactive surfaces append to, so a prompt typed in one is there in the other.
//!
//! The rules are bash's, because they are the ones people already have in their fingers:
//!
//! - a line starting with a space or tab is never recorded (the "don't remember this"
//!   escape hatch);
//! - a line identical to the one before it is not recorded twice;
//! - the file is capped at `history_size` entries, oldest dropped first.
//!
//! Storage is one entry per line. A prompt may itself span lines, so backslashes and
//! newlines are escaped on write and unescaped on read, which keeps the file
//! line-oriented while round-tripping a pasted block exactly.
//!
//! Every filesystem interaction is best-effort. A history file that is missing,
//! unreadable, half-written, or full of binary junk must never stop a REPL from
//! starting: loading degrades to "no history", appending to "this session only".

use crate::config::UiConfig;
use crate::session::expand_home;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `path` with `~` and `$VARS` expanded, as Python's `expanduser` plus `expandvars`.
pub fn history_path(path: &str) -> PathBuf {
    expand_home(&expand_vars(path))
}

/// Expand `$NAME` and `${NAME}` (and `%NAME%` on Windows), leaving unknown
/// variables untouched, as Python's `os.path.expandvars`.
fn expand_vars(text: &str) -> String {
    let lookup = |name: &str| std::env::var(name).ok();
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(['$', '%']) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let (name, consumed) = if let Some(braced) = tail.strip_prefix("${") {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 3),
                None => ("", 0),
            }
        } else if let Some(bare) = tail.strip_prefix('$') {
            let end = bare.find(|c: char| !is_name(c)).unwrap_or(bare.len());
            (&bare[..end], end + 1)
        } else if cfg!(windows) {
            let percent = &tail[1..];
            match percent.find('%') {
                Some(end) if end > 0 => (&percent[..end], end + 2),
                _ => ("", 0),
            }
        } else {
            ("", 0)
        };
        match (consumed, lookup(name).filter(|_| !name.is_empty())) {
            (0, _) | (_, None) => {
                let skip = if consumed == 0 { 1 } else { consumed };
                out.push_str(&tail[..skip]);
                rest = &tail[skip..];
            }
            (_, Some(value)) => {
                out.push_str(&value);
                rest = &tail[consumed..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One entry as one physical line (backslash and newline escaped).
fn encode(entry: &str) -> String {
    entry
        .replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace('\n', "\\n")
}

/// Inverse of [`encode`], tolerant of a trailing lone backslash.
fn decode(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                    continue;
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// Is this line worth remembering? (bash rules: no blanks, nothing leading-space.)
pub fn storable(line: &str) -> bool {
    !line.is_empty() && !line.starts_with([' ', '\t']) && !line.trim().is_empty()
}

/// The recorded prompts, oldest first, backed by a file (or memory only).
#[derive(Debug, Clone)]
pub struct PromptHistory {
    path: Option<PathBuf>,
    /// `0` disables the cap.
    max_entries: usize,
    entries: Vec<String>,
}

impl PromptHistory {
    /// History at `path` (`None` keeps it in memory), loaded from the file.
    pub fn new(path: Option<&str>, max_entries: i64) -> Self {
        let mut history = Self {
            path: path.filter(|path| !path.is_empty()).map(history_path),
            max_entries: usize::try_from(max_entries).unwrap_or(0),
            entries: Vec::new(),
        };
        history.load();
        history
    }

    /// History wired from `[ui] history_file` and `history_size`.
    pub fn from_config(ui: &UiConfig) -> Self {
        Self::new(Some(&ui.history_file), ui.history_size)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The recorded prompts, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// (Re)read the file. A missing or corrupt file yields no history; a bad byte
    /// costs that one line's fidelity, not the whole history.
    pub fn load(&mut self) {
        self.entries.clear();
        let Some(raw) = self.path.as_ref().and_then(|path| std::fs::read(path).ok()) else {
            return;
        };
        let raw = String::from_utf8_lossy(&raw);
        let mut entries: Vec<String> = Vec::new();
        for line in crate::difflib::splitlines(&raw) {
            let entry = decode(line);
            if !storable(&entry) || entry.contains('\0') {
                continue;
            }
            if entries.last() == Some(&entry) {
                continue;
            }
            entries.push(entry);
        }
        self.entries = entries;
        self.cap();
    }

    /// Record a submitted prompt; returns whether it was kept. Refused for a blank
    /// line, a leading-space line, or a repeat of the previous entry. A file that
    /// cannot be written is not an error: the entry still lives in memory.
    pub fn append(&mut self, line: &str) -> bool {
        if !storable(line) || self.entries.last().map(String::as_str) == Some(line) {
            return false;
        }
        self.entries.push(line.to_owned());
        if self.max_entries > 0 && self.entries.len() > self.max_entries {
            // Over the cap: rewrite the whole file, so it holds exactly what memory does.
            self.cap();
            self.rewrite();
        } else {
            self.append_line(line);
        }
        true
    }

    fn cap(&mut self) {
        if self.max_entries > 0 && self.entries.len() > self.max_entries {
            let excess = self.entries.len() - self.max_entries;
            self.entries.drain(..excess);
        }
    }

    /// Append one entry with a single write, so two sessions sharing the file
    /// interleave whole lines instead of overwriting each other's.
    fn append_line(&self, line: &str) {
        let Some(path) = &self.path else { return };
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(format!("{}\n", encode(line)).as_bytes())
        })();
    }

    /// Replace the file with the in-memory entries, atomically (temp file + rename).
    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let payload: String = self
            .entries
            .iter()
            .map(|entry| format!("{}\n", encode(entry)))
            .collect();
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temp = parent.join(format!(".history-{}", std::process::id()));
        if std::fs::write(&temp, payload).is_err() || std::fs::rename(&temp, path).is_err() {
            // Leave no stray temp file behind when the rename is what failed.
            let _ = std::fs::remove_file(&temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent86-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("history")
    }

    #[test]
    fn bash_rules_decide_what_is_recorded() {
        let mut history = PromptHistory::new(None, 1000);
        assert!(history.append("first"));
        assert!(!history.append("first"), "an immediate repeat is dropped");
        assert!(
            !history.append(" secret"),
            "a leading space is never recorded"
        );
        assert!(!history.append("\tsecret"));
        assert!(!history.append("   "));
        assert!(!history.append(""));
        assert!(history.append("second"));
        assert!(
            history.append("first"),
            "only *adjacent* repeats are dropped"
        );
        assert_eq!(history.entries(), ["first", "second", "first"]);
    }

    #[test]
    fn multi_line_prompts_round_trip_through_the_file() {
        let path = temp_file("roundtrip");
        let path_text = path.to_str().unwrap();
        let mut history = PromptHistory::new(Some(path_text), 1000);
        history.append("line one\nline two");
        history.append(r"C:\temp\new");
        history.append("crlf\r\nend");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw, "line one\\nline two\nC:\\\\temp\\\\new\ncrlf\\nend\n",
            "one physical line per entry"
        );
        let reloaded = PromptHistory::new(Some(path_text), 1000);
        assert_eq!(
            reloaded.entries(),
            ["line one\nline two", r"C:\temp\new", "crlf\nend"]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn the_cap_drops_the_oldest_and_rewrites_the_file() {
        let path = temp_file("cap");
        let path_text = path.to_str().unwrap();
        let mut history = PromptHistory::new(Some(path_text), 2);
        for entry in ["a", "b", "c"] {
            history.append(entry);
        }
        assert_eq!(history.entries(), ["b", "c"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b\nc\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn junk_in_the_file_is_skipped_not_fatal() {
        let path = temp_file("junk");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            b"good\n\n  spaced\nnul\x00byte\ngood\nbad \xff byte\ntrailing\\",
        )
        .unwrap();
        let history = PromptHistory::new(Some(path.to_str().unwrap()), 1000);
        assert_eq!(
            history.entries(),
            ["good", "bad \u{fffd} byte", "trailing\\"]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn variables_and_home_expand_in_the_path() {
        std::env::set_var("AGENT86_HISTORY_TEST_DIR", "from-env");
        let expanded = history_path("$AGENT86_HISTORY_TEST_DIR/${AGENT86_HISTORY_TEST_DIR}/x");
        assert_eq!(expanded, PathBuf::from("from-env/from-env/x"));
        assert_eq!(
            history_path("$AGENT86_HISTORY_UNSET_VAR/x"),
            PathBuf::from("$AGENT86_HISTORY_UNSET_VAR/x")
        );
        assert_eq!(history_path("cost $5"), PathBuf::from("cost $5"));
    }
}
