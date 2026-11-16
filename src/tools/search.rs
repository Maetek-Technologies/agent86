//! Beyond Python: `search_files` (a regex over file contents) and `find_files` (a
//! glob over paths), so the model can look around without `grep`, `find`, or `rg`
//! through `run_command`, which needs approval under `ask`, depends on what the
//! host has installed, and behaves differently on Windows.
//!
//! Both are read-only and `parallel_safe`, so they run in the parallel read path
//! with no approval; `dispatch` redacts their results through the vault, and the
//! observation guardrail applies, as for `read_file`.
//!
//! The walk (the `ignore` crate, ripgrep's walker) respects `.gitignore`,
//! `.ignore`, `.git/info/exclude`, and git's global excludes, whether or not the
//! workspace is a git repository. Hidden files and directories are searched, since
//! much of a project's configuration lives in them (`.github/`, `.cargo/`,
//! `.agent86/`); version-control metadata (`.git`, `.hg`, `.svn`, `.jj`) never is,
//! and secret files such as `.env` are left to `.gitignore`. Symlinks are never
//! descended into; a symlinked file is read only when its target lies inside the
//! jail root the walk started in, so a skill root added with
//! [`SandboxPolicy::allow`](super::SandboxPolicy::allow) is searched only when
//! named as `path`. Files whose first 8 KiB hold a NUL byte count as binary and
//! are skipped.
//!
//! Results come in path order (each directory's entries sorted by name, depth
//! first), with paths relative to the workspace and `/`-separated on every OS, so
//! they are stable across runs and platforms. Every bound that cuts a result says
//! so in a note, with how to narrow the search: large repositories make that
//! matter more than speed.

use super::{display_path, failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// How many results a call returns when the model does not say.
pub const DEFAULT_MAX_RESULTS: i64 = 200;
/// The most results a call may ask for.
pub const MAX_RESULTS: i64 = 2000;
/// The most context lines `search_files` shows on each side of a match.
pub const MAX_CONTEXT: i64 = 10;
/// Output lines longer than this many characters (minified files) are cut.
pub const MAX_LINE_CHARS: usize = 500;
/// How many files one call looks at before it stops.
pub const MAX_FILES: usize = 20_000;
/// Files larger than this are not searched.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// How much of a file is checked for a NUL byte to call it binary.
const BINARY_PROBE: usize = 8 * 1024;
/// Version-control metadata, never searched.
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];
/// Appended to an output line cut at [`MAX_LINE_CHARS`].
pub const LINE_MARKER: &str = " [... line truncated]";

/// `search_files`: a regex over the contents of files in the workspace.
pub struct SearchFilesTool;

/// `find_files`: a glob over the paths of files in the workspace.
pub struct FindFilesTool;

impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search the contents of files in the workspace for a regular expression (Rust \
         regex syntax) and return the matching lines as `path:line: text`, grouped by \
         file in path order. Respects .gitignore and skips version-control directories \
         and binary files. Prefer this to grep or rg through run_command."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "pattern": {
                    "description": "Regular expression to search for (Rust regex syntax), matched against each line.",
                    "title": "Pattern",
                    "type": "string",
                },
                "path": {
                    "default": ".",
                    "description": "Directory or file to search, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
                "glob": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                    "description": "Only search files whose paths match this glob, such as `src/**/*.rs`; a glob without `/` matches file names, such as `*.toml`.",
                    "title": "Glob",
                },
                "case_insensitive": {
                    "default": false,
                    "description": "Match regardless of case.",
                    "title": "Case Insensitive",
                    "type": "boolean",
                },
                "context": {
                    "default": 0,
                    "description": "Lines of context to show before and after each match.",
                    "maximum": MAX_CONTEXT,
                    "minimum": 0,
                    "title": "Context",
                    "type": "integer",
                },
                "max_results": {
                    "default": DEFAULT_MAX_RESULTS,
                    "description": "Maximum number of matching lines to return.",
                    "maximum": MAX_RESULTS,
                    "minimum": 1,
                    "title": "Max Results",
                    "type": "integer",
                },
            },
            "required": ["pattern"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let pattern = arguments.string("pattern", None);
        let path = arguments.string("path", Some("."));
        let glob = arguments.optional_string("glob");
        let case_insensitive = arguments.boolean("case_insensitive", false);
        let context = arguments.bounded_integer("context", 0, 0, MAX_CONTEXT);
        let max_results =
            arguments.bounded_integer("max_results", DEFAULT_MAX_RESULTS, 1, MAX_RESULTS);
        arguments.finish()?;

        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|error| {
                ToolError::InvalidArguments(vec![format!(
                    "pattern: Invalid regex: {}",
                    regex_message(&error)
                )])
            })?;
        let glob = glob.as_deref().map(PathGlob::new).transpose()?;
        let start = match Start::resolve(self.name(), ctx, &path)? {
            Ok(start) => start,
            Err(result) => return Ok(result),
        };
        if let Some(dir) = start.vcs_dir() {
            return Ok(success(self.name(), vcs_note("No matches", &path, &dir)));
        }

        let context = usize::try_from(context).unwrap_or(0);
        let max_results = usize::try_from(max_results).unwrap_or(1);
        let mut out: Vec<String> = Vec::new();
        let mut matches = 0usize;
        let mut files_matched = 0usize;
        let mut examined = 0usize;
        let mut too_large = 0usize;
        let mut results_capped = false;
        let mut files_capped = false;

        for file in start.files() {
            let shown = shown(ctx, &file);
            if glob.as_ref().is_some_and(|glob| !glob.matches(&shown)) {
                continue;
            }
            if examined == MAX_FILES {
                files_capped = true;
                break;
            }
            examined += 1;
            let text = match read_text(&file) {
                Text::Lines(text) => text,
                Text::TooLarge => {
                    too_large += 1;
                    continue;
                }
                Text::Skipped => continue,
            };
            let lines = split_lines(&text);
            let mut hits = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    if matches + hits.len() == max_results {
                        results_capped = true;
                        break;
                    }
                    hits.push(index);
                }
            }
            if !hits.is_empty() {
                if context > 0 && !out.is_empty() {
                    out.push("--".to_owned());
                }
                render_hits(&mut out, &shown, &lines, &hits, context);
                matches += hits.len();
                files_matched += 1;
            }
            if results_capped {
                break;
            }
        }

        let mut notes = Vec::new();
        if results_capped {
            notes.push(capped_note(
                max_results,
                "matching lines",
                "a more specific pattern, a smaller `path`, or a `glob`",
            ));
        }
        if files_capped {
            notes.push(files_note("a smaller `path` or a `glob`"));
        }
        if too_large > 0 {
            notes.push(large_note(too_large));
        }
        let body = assemble(out, "No matches.", &notes);
        let mut result = success(self.name(), ctx.policy.truncate(body));
        result.metadata.insert("matches".into(), json!(matches));
        result.metadata.insert("files".into(), json!(files_matched));
        result
            .metadata
            .insert("truncated".into(), json!(results_capped || files_capped));
        Ok(result)
    }
}

impl Tool for FindFilesTool {
    fn name(&self) -> &str {
        "find_files"
    }

    fn description(&self) -> &str {
        "Find files in the workspace whose paths match a glob, such as `src/**/*.rs` (a \
         glob without `/` matches file names anywhere, such as `*.toml`), and return \
         their paths in path order. Respects .gitignore and skips version-control \
         directories. Prefer this to find or ls through run_command."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "pattern": {
                    "description": "Glob to match against paths relative to the workspace, such as `src/**/*.rs`; a glob without `/` matches file names, such as `*.toml`.",
                    "title": "Pattern",
                    "type": "string",
                },
                "path": {
                    "default": ".",
                    "description": "Directory to search, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
                "max_results": {
                    "default": DEFAULT_MAX_RESULTS,
                    "description": "Maximum number of paths to return.",
                    "maximum": MAX_RESULTS,
                    "minimum": 1,
                    "title": "Max Results",
                    "type": "integer",
                },
            },
            "required": ["pattern"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let pattern = arguments.string("pattern", None);
        let path = arguments.string("path", Some("."));
        let max_results =
            arguments.bounded_integer("max_results", DEFAULT_MAX_RESULTS, 1, MAX_RESULTS);
        arguments.finish()?;

        let glob = PathGlob::new_for("pattern", &pattern)?;
        let start = match Start::resolve(self.name(), ctx, &path)? {
            Ok(start) => start,
            Err(result) => return Ok(result),
        };
        if let Some(dir) = start.vcs_dir() {
            return Ok(success(
                self.name(),
                vcs_note("No files matched", &path, &dir),
            ));
        }

        let max_results = usize::try_from(max_results).unwrap_or(1);
        let mut out = Vec::new();
        let mut results_capped = false;
        let mut files_capped = false;
        for (visited, file) in start.files().enumerate() {
            if visited == MAX_FILES {
                files_capped = true;
                break;
            }
            let shown = shown(ctx, &file);
            if glob.matches(&shown) {
                if out.len() == max_results {
                    results_capped = true;
                    break;
                }
                out.push(shown);
            }
        }

        let found = out.len();
        let mut notes = Vec::new();
        if results_capped {
            notes.push(capped_note(
                max_results,
                "paths",
                "a more specific pattern or a smaller `path`",
            ));
        }
        if files_capped {
            notes.push(files_note("a smaller `path`"));
        }
        let body = assemble(out, "No files matched.", &notes);
        let mut result = success(self.name(), ctx.policy.truncate(body));
        result.metadata.insert("files".into(), json!(found));
        result
            .metadata
            .insert("truncated".into(), json!(results_capped || files_capped));
        Ok(result)
    }
}

/// `path` as the search tools show it: relative to the workspace with `/`
/// separators on every OS, or, under an allowed root outside the workspace (a
/// skill directory), absolute with `/` separators.
fn shown(ctx: &ToolContext, path: &Path) -> String {
    match path.strip_prefix(ctx.policy.workspace()) {
        Ok(relative) => relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => display_path(path).replace('\\', "/"),
    }
}

/// Where a walk starts: the resolved `path`, and the jail root it lies under.
struct Start {
    path: PathBuf,
    root: PathBuf,
}

impl Start {
    /// Resolve `path` through the jail. The inner `Err` is a result for the model
    /// (the path does not exist).
    fn resolve(
        tool: &str,
        ctx: &ToolContext,
        path: &str,
    ) -> Result<Result<Self, ToolResult>, ToolError> {
        let target = ctx.policy.resolve_within(path)?;
        if !target.exists() {
            return Ok(Err(failure(tool, format!("Path not found: {path}"))));
        }
        let root = ctx
            .policy
            .root_of(&target)
            .unwrap_or_else(|| ctx.policy.workspace())
            .to_path_buf();
        Ok(Ok(Self { path: target, root }))
    }

    /// The version-control directory the start lies in, if any.
    fn vcs_dir(&self) -> Option<String> {
        let inside = self.path.strip_prefix(&self.root).ok()?;
        inside.components().find_map(|component| match component {
            Component::Normal(name) if is_vcs(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
    }

    /// The files under the start, in path order, with ignored files, version-control
    /// metadata, and symlinks that leave the root left out.
    fn files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        let mut builder = WalkBuilder::new(&self.path);
        builder
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|entry| entry.depth() == 0 || !is_vcs(entry.file_name()));
        builder.build().filter_map(move |entry| {
            let entry = entry.ok()?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                // Never descended into; a linked file only when it stays in the root.
                let real = std::fs::canonicalize(entry.path()).ok()?;
                if !(real.starts_with(&self.root) && real.is_file()) {
                    return None;
                }
            } else if !kind.is_file() {
                return None;
            }
            Some(entry.into_path())
        })
    }
}

fn is_vcs(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    VCS_DIRS.iter().any(|vcs| vcs.eq_ignore_ascii_case(&name))
}

/// A glob over the paths the tools show. One without `/` matches the file name,
/// as ripgrep's `-g` does; backslashes are read as separators, since a model on
/// Windows may write them.
struct PathGlob {
    matcher: GlobMatcher,
    whole_path: bool,
}

impl PathGlob {
    fn new(pattern: &str) -> Result<Self, ToolError> {
        Self::new_for("glob", pattern)
    }

    fn new_for(field: &str, pattern: &str) -> Result<Self, ToolError> {
        let pattern = pattern.replace('\\', "/");
        let pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .map_err(|error| {
                ToolError::InvalidArguments(vec![format!("{field}: Invalid glob: {error}")])
            })?
            .compile_matcher();
        Ok(Self {
            matcher,
            whole_path: pattern.contains('/'),
        })
    }

    fn matches(&self, shown: &str) -> bool {
        if self.whole_path {
            self.matcher.is_match(shown)
        } else {
            self.matcher
                .is_match(shown.rsplit('/').next().unwrap_or(shown))
        }
    }
}

enum Text {
    Lines(String),
    TooLarge,
    Skipped,
}

/// A file's text, or why it is not searched (binary, unreadable, too large).
fn read_text(path: &Path) -> Text {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => return Text::TooLarge,
        Ok(_) => {}
        Err(_) => return Text::Skipped,
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Text::Skipped;
    };
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return Text::Skipped;
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Text::Lines(String::from_utf8_lossy(body).into_owned())
}

/// `text`'s lines, without their `\n` or `\r\n`.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Matches as `path:line: text`, and context lines as `path-line- text`, with
/// `--` between groups that do not touch, as grep does.
fn render_hits(out: &mut Vec<String>, shown: &str, lines: &[&str], hits: &[usize], context: usize) {
    let mut last: Option<usize> = None;
    for (position, &hit) in hits.iter().enumerate() {
        let from = hit.saturating_sub(context);
        let from = last.map_or(from, |last| from.max(last + 1));
        if let Some(last) = last {
            if from > last + 1 {
                out.push("--".to_owned());
            }
        }
        let next = hits.get(position + 1).copied();
        let mut to = (hit + context).min(lines.len() - 1);
        if let Some(next) = next {
            to = to.min(next.saturating_sub(1).max(hit));
        }
        for (index, line) in lines.iter().enumerate().take(to + 1).skip(from) {
            let separator = if index == hit { ':' } else { '-' };
            out.push(format!(
                "{shown}{separator}{}{separator} {}",
                index + 1,
                clip(line)
            ));
        }
        last = Some(to);
    }
}

/// `line` cut at [`MAX_LINE_CHARS`] characters, with [`LINE_MARKER`].
fn clip(line: &str) -> Cow<'_, str> {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => Cow::Owned(format!("{}{LINE_MARKER}", &line[..cut])),
        None => Cow::Borrowed(line),
    }
}

fn assemble(out: Vec<String>, empty: &str, notes: &[String]) -> String {
    let mut body = if out.is_empty() {
        empty.to_owned()
    } else {
        out.join("\n")
    };
    for note in notes {
        body.push_str("\n\n");
        body.push_str(note);
    }
    body
}

fn capped_note(max_results: usize, what: &str, narrow: &str) -> String {
    let raise = if max_results < MAX_RESULTS as usize {
        format!(", or raise `max_results` (at most {MAX_RESULTS})")
    } else {
        String::new()
    };
    format!("[Showing the first {max_results} {what}. Narrow the search with {narrow}{raise}.]")
}

fn files_note(narrow: &str) -> String {
    format!("[Stopped after looking at {MAX_FILES} files. Narrow the search with {narrow}.]")
}

fn large_note(count: usize) -> String {
    let files = if count == 1 { "file" } else { "files" };
    format!(
        "[Skipped {count} {files} over {} MiB.]",
        MAX_FILE_BYTES / (1024 * 1024)
    )
}

fn vcs_note(lead: &str, path: &str, dir: &str) -> String {
    format!("{lead}: '{path}' is inside version-control metadata ({dir}), which is not searched.")
}

/// The last line of a regex error (the parser's message, without the pattern
/// and caret drawn above it).
fn regex_message(error: &regex::Error) -> String {
    let text = error.to_string();
    let last = text.lines().rev().find(|line| !line.trim().is_empty());
    last.unwrap_or(&text)
        .trim()
        .trim_start_matches("error: ")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_groups_merge_and_split_like_grep() {
        let lines = ["a", "hit", "b", "c", "d", "e", "hit", "f"];
        let mut out = Vec::new();
        render_hits(&mut out, "x", &lines, &[1, 6], 1);
        assert_eq!(
            out,
            ["x-1- a", "x:2: hit", "x-3- b", "--", "x-6- e", "x:7: hit", "x-8- f"]
        );

        let mut out = Vec::new();
        render_hits(&mut out, "x", &lines, &[1, 3], 2);
        assert_eq!(
            out,
            ["x-1- a", "x:2: hit", "x-3- b", "x:4: c", "x-5- d", "x-6- e"]
        );
    }

    #[test]
    fn lines_split_on_both_newline_styles() {
        assert_eq!(split_lines("a\r\nb\nc\n"), ["a", "b", "c"]);
        assert!(split_lines("").is_empty());
        assert_eq!(split_lines("\n"), [""]);
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let line = "é".repeat(MAX_LINE_CHARS + 5);
        let clipped = clip(&line);
        assert!(clipped.ends_with(LINE_MARKER));
        assert_eq!(
            clipped.trim_end_matches(LINE_MARKER).chars().count(),
            MAX_LINE_CHARS
        );
    }
}
