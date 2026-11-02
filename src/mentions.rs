//! `@file` mentions, ported from Python's `tui/mentions.py`: pulling a file into
//! the prompt without asking for a tool call.
//!
//! Typing `@src/types.rs` puts that file in front of the model directly: no round
//! trip, no `read_file` step. Two hard rules, because a mention is user text that
//! makes the harness read the disk:
//!
//! - every mentioned path goes through [`SandboxPolicy::resolve_within`], so a mention
//!   can never reach outside the workspace jail. A refused path is reported and never
//!   opened, not even to check its size or type;
//! - what gets inlined is bounded: a size cap per file (`tools.mention_max_bytes`), at
//!   most [`MAX_DIR_ENTRIES`] names for a directory, and binaries refused outright.

use crate::tools::{SandboxPolicy, ToolError};
use std::path::{Path, PathBuf};

/// Most names listed for a mentioned directory.
pub const MAX_DIR_ENTRIES: usize = 200;
/// Most completions offered for a partial `@` path.
pub const MAX_COMPLETIONS: usize = 20;

/// Directories never worth listing: huge, generated, or not source.
pub const SKIP_DIRS: [&str; 10] = [
    ".git",
    ".venv",
    "venv",
    "node_modules",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".idea",
];

/// Trailing characters stripped off an unquoted path: prose punctuation, not
/// filename. `.` is deliberately absent; it is far more often an extension.
const TRAILING_PUNCT: &[char] = &[',', ';', ':', '!', '?', ')', ']', '}', '>'];

/// Bytes sniffed when deciding whether a file is binary.
const SNIFF_BYTES: usize = 8192;

/// What one prompt's mentions expanded to.
///
/// `prompt` is what to send the model: the user's text, then one block per mention.
/// `errors` are why some were not attached; a surface shows them to the user, and
/// they are in `prompt` too, so the model knows a promised file is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionResult {
    pub prompt: String,
    pub attachments: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// Python's `\s` for `str` patterns: Unicode whitespace plus U+001C to U+001F.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// The raw paths mentioned in `text`, in order, without duplicates.
///
/// Python's pattern is `(?<![^\s(\[{])@(?:"([^"\n]+)"|([^\s"'`]+))`: the `@` must
/// start the text or follow whitespace or an opening bracket, so `user@example.com`
/// is an address, not a mention of `example.com`.
pub fn find_mentions(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let starts = c == '@' && previous.is_none_or(|p| is_space(p) || "([{".contains(p));
        previous = Some(c);
        if !starts {
            continue;
        }
        let rest = &text[index + 1..];
        let (raw, consumed) = if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.find(['"', '\n']) {
                Some(end) if end > 0 && quoted[end..].starts_with('"') => {
                    (quoted[..end].to_owned(), end + 2)
                }
                _ => continue,
            }
        } else {
            let end = rest
                .find(|c: char| is_space(c) || "\"'`".contains(c))
                .unwrap_or(rest.len());
            if end == 0 {
                continue;
            }
            (rest[..end].trim_end_matches(TRAILING_PUNCT).to_owned(), end)
        };
        // Matches never overlap: scanning resumes after this one.
        let stop = index + 1 + consumed;
        while chars.peek().is_some_and(|&(next, _)| next < stop) {
            previous = chars.next().map(|(_, c)| c);
        }
        if !raw.is_empty() && !found.contains(&raw) {
            found.push(raw);
        }
    }
    found
}

/// Expand every `@path` in `text` into an inline block, jailed by `policy`.
pub fn expand_mentions(text: &str, policy: &SandboxPolicy, max_bytes: u64) -> MentionResult {
    let raws = find_mentions(text);
    let mut result = MentionResult {
        prompt: text.to_owned(),
        attachments: Vec::new(),
        errors: Vec::new(),
    };
    if raws.is_empty() {
        return result;
    }
    let mut blocks = Vec::with_capacity(raws.len());
    for raw in &raws {
        match expand_one(raw, policy, max_bytes) {
            Ok((block, path)) => {
                blocks.push(block);
                result.attachments.push(path);
            }
            Err(reason) => {
                blocks.push(format!("--- @{raw} (not attached) ---\n{reason}"));
                result.errors.push(format!("@{raw}: {reason}"));
            }
        }
    }
    result.prompt = format!("{}\n\n{}", text.trim_end(), blocks.join("\n\n"));
    result
}

/// Render one mention: its block and the inlined path, or why it was not attached.
fn expand_one(
    raw: &str,
    policy: &SandboxPolicy,
    max_bytes: u64,
) -> Result<(String, PathBuf), String> {
    if raw.contains('\0') {
        return Err("unreadable path (ValueError)".to_owned());
    }
    // Deliberately not echoing the resolved location: the refusal says where the
    // jail is, never what happens to exist outside it.
    let resolved = policy
        .resolve_within(raw)
        .map_err(|_| "refused - resolves outside the workspace".to_owned())?;
    let unreadable = |error: std::io::Error| {
        let exception = match ToolError::io(&error) {
            ToolError::Raised { exception, .. } => exception,
            ToolError::InvalidArguments(_) => "OSError".to_owned(),
        };
        format!("could not be read ({exception}: {error})")
    };
    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("no such file or directory".to_owned())
        }
        Err(error) => return Err(unreadable(error)),
    };
    let label = display(&resolved, policy, raw);
    if metadata.is_dir() {
        let block = expand_dir(&resolved, &label).map_err(unreadable)?;
        return Ok((block, resolved));
    }
    // Beyond Python: a FIFO, socket, or device reports no size and could be read
    // forever, so only regular files are inlined.
    if !metadata.is_file() {
        return Err("not a regular file".to_owned());
    }
    let size = metadata.len();
    let over_cap = |size: u64| {
        format!(
            "{size} bytes is over the {max_bytes}-byte mention cap; \
             read it with a tool instead of inlining it"
        )
    };
    if max_bytes > 0 && size > max_bytes {
        return Err(over_cap(size));
    }
    // Read no further than the cap, in case the file grew since it was measured.
    use std::io::Read;
    let file = std::fs::File::open(&resolved).map_err(unreadable)?;
    let mut reader: Box<dyn Read> = if max_bytes > 0 {
        Box::new(file.take(max_bytes + 1))
    } else {
        Box::new(file)
    };
    let mut data = Vec::new();
    reader.read_to_end(&mut data).map_err(unreadable)?;
    if max_bytes > 0 && data.len() as u64 > max_bytes {
        return Err(over_cap(data.len() as u64));
    }
    let text = decode(&data).ok_or_else(|| "looks like a binary file".to_owned())?;
    let lines = crate::difflib::splitlines(&text).len();
    let header = format!("--- @{label} ({lines} lines) ---");
    Ok((format!("{header}\n{}", fenced(&text)), resolved))
}

fn expand_dir(resolved: &Path, label: &str) -> std::io::Result<String> {
    let mut children: Vec<(bool, String)> = std::fs::read_dir(resolved)?
        .filter_map(Result::ok)
        .map(|entry| {
            let is_dir = entry.path().is_dir();
            (is_dir, entry.file_name().to_string_lossy().into_owned())
        })
        .filter(|(_, name)| !SKIP_DIRS.contains(&name.as_str()))
        .collect();
    children.sort_by_key(|(is_dir, name)| (!is_dir, name.to_lowercase()));
    let names: Vec<String> = children
        .into_iter()
        .map(|(is_dir, name)| if is_dir { format!("{name}/") } else { name })
        .collect();
    let shown = &names[..names.len().min(MAX_DIR_ENTRIES)];
    let mut body = if shown.is_empty() {
        "(empty)".to_owned()
    } else {
        shown.join("\n")
    };
    if names.len() > shown.len() {
        body.push_str(&format!("\n... and {} more", names.len() - shown.len()));
    }
    let header = format!("--- @{label} (directory, {} entries) ---", names.len());
    Ok(format!("{header}\n{}", fenced(&body)))
}

/// `data` as text, or `None` if it is binary (a NUL early on, or not UTF-8).
fn decode(data: &[u8]) -> Option<String> {
    if data[..data.len().min(SNIFF_BYTES)].contains(&0) {
        return None;
    }
    String::from_utf8(data.to_vec()).ok()
}

/// `body` in a code fence long enough to survive backticks inside it.
fn fenced(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}\n{}\n{fence}", body.trim_end_matches('\n'))
}

/// The path as the user should see it: workspace-relative when it can be.
fn display(resolved: &Path, policy: &SandboxPolicy, raw: &str) -> String {
    match resolved.strip_prefix(policy.workspace()) {
        Ok(relative) => {
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            if parts.is_empty() {
                ".".to_owned()
            } else {
                parts.join("/")
            }
        }
        Err(_) => raw.to_owned(),
    }
}

/// Up to [`MAX_COMPLETIONS`] workspace paths matching a partial `@` mention.
///
/// `prefix` is what follows the `@` (a leading `@` and wrapping quotes are
/// tolerated). Results are workspace-relative and `/`-separated, directories first
/// and ending in `/` so completing again descends into them. Noise directories
/// ([`SKIP_DIRS`]) and dotfiles are hidden unless the prefix names them.
pub fn complete_mentions(prefix: &str, workspace: &Path) -> Vec<String> {
    let cleaned = prefix
        .trim()
        .trim_start_matches('@')
        .trim_matches('"')
        .replace('\\', "/");
    let (head, tail) = match cleaned.rfind('/') {
        Some(at) => (Some(&cleaned[..at]), &cleaned[at + 1..]),
        None => (None, cleaned.as_str()),
    };
    let Ok(root) = std::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let directory = head.map_or_else(|| root.clone(), |head| root.join(head));
    // Never complete outside the workspace, whatever `../..` the prefix holds.
    let Ok(directory) = std::fs::canonicalize(directory) else {
        return Vec::new();
    };
    if !directory.starts_with(&root) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut children: Vec<(bool, String)> = entries
        .filter_map(Result::ok)
        .map(|entry| {
            // A broken link or a permission wall is completed as a file.
            let is_dir = entry.path().is_dir();
            (is_dir, entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    children.sort_by_key(|(is_dir, name)| (!is_dir, name.to_lowercase()));

    let needle = tail.to_lowercase();
    children
        .into_iter()
        .filter(|(_, name)| {
            let lower = name.to_lowercase();
            lower.starts_with(&needle)
                && (!SKIP_DIRS.contains(&name.as_str()) || needle == lower)
                && (!name.starts_with('.') || needle.starts_with('.'))
        })
        .take(MAX_COMPLETIONS)
        .map(|(is_dir, name)| {
            let path = head.map_or_else(|| name.clone(), |head| format!("{head}/{name}"));
            if is_dir {
                format!("{path}/")
            } else {
                path
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_list_directories_first_and_stay_in_the_workspace() {
        let dir = std::env::temp_dir().join(format!("agent86-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.as_path();
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("Setup.py"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/Lib.rs"), "").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();

        assert_eq!(complete_mentions("", root), ["src/", "Setup.py"]);
        assert_eq!(complete_mentions("s", root), ["src/", "Setup.py"]);
        assert_eq!(
            complete_mentions("@src/", root),
            ["src/ui/", "src/Lib.rs", "src/main.rs"]
        );
        assert_eq!(complete_mentions("\"src\\m", root), ["src/main.rs"]);
        // `.git` is noise even when dotfiles are asked for, unless named in full.
        assert_eq!(complete_mentions(".", root), [".env"]);
        assert_eq!(complete_mentions(".git", root), [".git/"]);
        assert_eq!(complete_mentions("node_modules", root), ["node_modules/"]);
        assert!(complete_mentions("../", root).is_empty());
        assert!(complete_mentions("missing/", root).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mentions_follow_pythons_pattern() {
        assert_eq!(find_mentions("see @a.py and @b/c.rs."), ["a.py", "b/c.rs."]);
        assert_eq!(find_mentions("mail user@example.com"), Vec::<String>::new());
        assert_eq!(find_mentions("(@x), [@y] {@z}"), ["x", "y", "z"]);
        assert_eq!(
            find_mentions("@\"my file.txt\" @a @a"),
            ["my file.txt", "a"]
        );
        assert_eq!(find_mentions("@\"open @b"), ["b"]);
        assert_eq!(find_mentions("@ alone, @'q'"), Vec::<String>::new());
        assert_eq!(find_mentions("@a@b @c"), ["a@b", "c"]);
        assert_eq!(find_mentions("@x?!"), ["x"]);
        assert_eq!(find_mentions("@),"), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_rather_than_read_forever() {
        let dir = std::env::temp_dir().join(format!("agent86-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("pipe"))
            .status()
            .is_ok_and(|status| status.success());
        if made {
            let policy = SandboxPolicy::new(&dir).unwrap();
            assert_eq!(
                expand_one("pipe", &policy, 200_000).unwrap_err(),
                "not a regular file"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let dir = std::env::temp_dir().join(format!("agent86-mention-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(20)).unwrap();
        let policy = SandboxPolicy::new(&dir).unwrap();
        assert_eq!(
            expand_one("big.txt", &policy, 10).unwrap_err(),
            "20 bytes is over the 10-byte mention cap; read it with a tool instead of inlining it"
        );
        assert!(expand_one("big.txt", &policy, 20).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fences_outgrow_the_backticks_inside() {
        assert_eq!(fenced("plain\n"), "```\nplain\n```");
        assert_eq!(fenced("a ```` b"), "`````\na ```` b\n`````");
    }
}
