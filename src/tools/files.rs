//! File tools, ported from Python's `tools/builtin/files.py`: read, write, edit,
//! and list, all jailed to the workspace by `SandboxPolicy::resolve_within`.
//!
//! Every mutation answers with a unified diff rather than a bare "wrote N bytes":
//! the diff is what the approval prompt shows before the write and what the
//! model reads back afterwards, so both judge the same artifact.
//!
//! `edit_file` preserves bytes: it remembers a file's BOM and CRLF line endings
//! and writes them back, so a one-line edit does not rewrite every line.

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::difflib::{splitlines, unified_diff};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// How many lines of a brand-new file the approval preview shows.
pub const PREVIEW_HEAD_LINES: usize = 40;
/// Lines of unchanged context either side of a hunk.
pub const DIFF_CONTEXT_LINES: usize = 3;

const BOM: &[u8] = b"\xef\xbb\xbf";

/// A unified diff from `old` to `new`, labelled with `path`; empty when identical.
pub fn unified_diff_for(path: &str, old: &str, new: &str) -> String {
    unified_diff(
        &splitlines(old),
        &splitlines(new),
        &format!("a/{path}"),
        &format!("b/{path}"),
        DIFF_CONTEXT_LINES,
    )
}

/// The first `limit` lines of `text`, with a marker when anything was dropped.
pub fn head_of(text: &str, limit: usize) -> String {
    let lines = splitlines(text);
    if lines.len() <= limit {
        return text.to_owned();
    }
    format!(
        "{}\n... [{} more lines]",
        lines[..limit].join("\n"),
        lines.len() - limit
    )
}

/// What a write to a not-yet-existing `path` would create.
pub fn new_file_summary(path: &str, content: &str) -> String {
    let count = splitlines(content).len();
    format!(
        "new file: {path} ({count} line{})\n{}",
        plural_s(count),
        head_of(content, PREVIEW_HEAD_LINES)
    )
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// A decoded text file plus the two facts needed to write it back unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileText {
    /// Newline-normalised to `\n` (for a consistently CRLF file), BOM stripped.
    pub text: String,
    pub crlf: bool,
    pub bom: bool,
}

impl FileText {
    /// Re-encode `text` with this file's original line endings and BOM.
    pub fn encode(&self, text: &str) -> Vec<u8> {
        let text = if self.crlf {
            text.replace('\n', "\r\n")
        } else {
            text.to_owned()
        };
        let mut bytes = Vec::with_capacity(text.len() + BOM.len());
        if self.bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }
}

#[derive(Debug)]
pub enum ReadTextError {
    NotUtf8,
    Io(std::io::Error),
}

/// Decode `target` as strict UTF-8, remembering its BOM and line endings.
///
/// Only a consistently CRLF file is normalised; a mixed file is matched and
/// written verbatim, because converting its lone LFs would touch lines nobody
/// asked to change.
pub fn read_file_text(target: &Path) -> Result<FileText, ReadTextError> {
    let data = std::fs::read(target).map_err(ReadTextError::Io)?;
    let bom = data.starts_with(BOM);
    let body = if bom { &data[BOM.len()..] } else { &data[..] };
    let text = String::from_utf8(body.to_vec()).map_err(|_| ReadTextError::NotUtf8)?;
    let crlf = text.matches("\r\n").count();
    if crlf > 0 && crlf == text.matches('\n').count() {
        return Ok(FileText {
            text: text.replace("\r\n", "\n"),
            crlf: true,
            bom,
        });
    }
    Ok(FileText {
        text,
        crlf: false,
        bom,
    })
}

/// The current contents of `target` as text, or `None` if absent or unreadable.
fn existing_text(target: &Path) -> Option<String> {
    if !target.is_file() {
        return None;
    }
    read_file_text(target).ok().map(|file| file.text)
}

/// Write `content` the way Python's `Path.write_text(encoding="utf-8")` does,
/// including its text-mode newline translation (`\n` becomes `\r\n` on Windows).
fn write_text(target: &Path, content: &str) -> std::io::Result<()> {
    if cfg!(windows) {
        std::fs::write(target, content.replace('\n', "\r\n"))
    } else {
        std::fs::write(target, content)
    }
}

/// Resolve a path for a preview: never failing, `None` when it cannot resolve.
fn preview_target(path: &str, ctx: &ToolContext) -> Option<PathBuf> {
    ctx.policy.resolve_within(path).ok()
}

fn string_arg<'a>(args: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| args.get(*name))
        .and_then(Value::as_str)
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file within the workspace and return its contents."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "path": {
                    "description": "Path to the file, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
            },
            "required": ["path"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let path = arguments.string("path", None);
        arguments.finish()?;

        let target = ctx.policy.resolve_within(&path)?;
        if !target.exists() {
            return Ok(failure(self.name(), format!("No such file: {path}")));
        }
        if !target.is_file() {
            return Ok(failure(self.name(), format!("Not a file: {path}")));
        }
        let bytes = std::fs::read(&target).map_err(|error| ToolError::io(&error))?;
        Ok(success(
            self.name(),
            ctx.policy.truncate(decode_text(&bytes)),
        ))
    }
}

/// Decode like Python's `read_text(encoding="utf-8", errors="replace")`.
///
/// Invalid UTF-8 becomes U+FFFD, a BOM is kept, and universal newlines apply:
/// `\r\n` and lone `\r` both become `\n`.
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a text file within the workspace (creates parent dirs). \
         Returns a unified diff of what changed."
    }

    fn side_effecting(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "path": {
                    "description": "Path to write, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
                "content": {
                    "description": "Full file contents to write.",
                    "title": "Content",
                    "type": "string",
                },
            },
            "required": ["path", "content"],
            "type": "object",
        })
    }

    fn preview(&self, args: &Map<String, Value>, ctx: &ToolContext) -> Option<String> {
        let path = string_arg(args, &["path"])?;
        let content = string_arg(args, &["content"])?;
        let current = preview_target(path, ctx).and_then(|target| existing_text(&target));
        let Some(current) = current else {
            return Some(new_file_summary(path, content));
        };
        let diff = unified_diff_for(path, &current, content);
        Some(if diff.is_empty() {
            format!("{path} is already exactly this content (no change).")
        } else {
            diff
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let path = arguments.string("path", None);
        let content = arguments.string("content", None);
        arguments.finish()?;

        let target = ctx.policy.resolve_within(&path)?;
        let existed = existing_text(&target);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| ToolError::io(&error))?;
        }
        write_text(&target, &content).map_err(|error| ToolError::io(&error))?;

        let (detail, diff) = match &existed {
            None => {
                let count = splitlines(&content).len();
                (
                    format!("new file, {count} line{}", plural_s(count)),
                    String::new(),
                )
            }
            Some(old) => {
                let diff = unified_diff_for(&path, old, &content);
                let detail = if diff.is_empty() {
                    "no change".to_owned()
                } else {
                    diff.clone()
                };
                (detail, diff)
            }
        };
        let kind = if existed.is_none() {
            "new file"
        } else {
            "overwrite"
        };
        let header = format!(
            "Wrote {} bytes to {path} ({kind}).",
            content.chars().count()
        );
        let mut result = success(
            self.name(),
            ctx.policy.truncate(format!("{header}\n{detail}")),
        );
        result.metadata.insert("path".to_owned(), json!(path));
        result.metadata.insert("diff".to_owned(), json!(diff));
        result
            .metadata
            .insert("created".to_owned(), json!(existed.is_none()));
        Ok(result)
    }
}

pub struct EditFileTool;

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a workspace file and return a unified diff of the \
         change. `old_string` must match the file exactly (whitespace included) and must be \
         unique unless `replace_all` is true."
    }

    fn side_effecting(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "path": {
                    "description": "Path to the file, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
                "old_string": {
                    "description": "Exact text to replace, including indentation and line breaks.",
                    "title": "Old String",
                    "type": "string",
                },
                "new_string": {
                    "description": "Replacement text.",
                    "title": "New String",
                    "type": "string",
                },
                "replace_all": {
                    "default": false,
                    "description": "Replace every occurrence instead of failing when there is more than one.",
                    "title": "Replace All",
                    "type": "boolean",
                },
            },
            "required": ["path", "old_string", "new_string"],
            "type": "object",
        })
    }

    fn preview(&self, args: &Map<String, Value>, ctx: &ToolContext) -> Option<String> {
        let path = string_arg(args, &["path"])?;
        let old = string_arg(args, &["old_string", "old"])?;
        let new = string_arg(args, &["new_string", "new"])?;
        let Some(current) = preview_target(path, ctx).and_then(|target| existing_text(&target))
        else {
            return Some(format!(
                "{path}: cannot be read (missing, or not a UTF-8 text file)."
            ));
        };
        let count = if old.is_empty() {
            current.chars().count() + 1
        } else {
            current.matches(old).count()
        };
        if count == 0 {
            return Some(format!(
                "{path}: old_string was NOT found — this edit would fail."
            ));
        }
        let replace_all = args.get("replace_all").is_some_and(truthy);
        if count > 1 && !replace_all {
            return Some(format!(
                "{path}: old_string is ambiguous — {count} matches, and replace_all is off."
            ));
        }
        let updated = if replace_all {
            current.replace(old, new)
        } else {
            current.replacen(old, new, 1)
        };
        let diff = unified_diff_for(path, &current, &updated);
        Some(if count > 1 {
            format!("{count} occurrences replaced\n{diff}")
        } else if diff.is_empty() {
            format!("{path}: old_string and new_string are identical (no change).")
        } else {
            diff
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let path = arguments.string("path", None);
        let old = arguments.string_alias(&["old_string", "old"]);
        let new = arguments.string_alias(&["new_string", "new"]);
        let replace_all = arguments.boolean("replace_all", false);
        arguments.finish()?;

        let target = ctx.policy.resolve_within(&path)?;
        if !target.is_file() {
            return Ok(failure(self.name(), format!("No such file: {path}")));
        }
        if old.is_empty() {
            return Ok(failure(
                self.name(),
                "old_string must not be empty; use write_file to create a file.".to_owned(),
            ));
        }
        let document = match read_file_text(&target) {
            Ok(document) => document,
            Err(ReadTextError::NotUtf8) => {
                return Ok(failure(
                    self.name(),
                    format!("Not a UTF-8 text file: {path}"),
                ));
            }
            Err(ReadTextError::Io(error)) => {
                return Ok(failure(
                    self.name(),
                    format!("Could not read {path}: {error}"),
                ));
            }
        };

        let count = document.text.matches(old.as_str()).count();
        if count == 0 {
            return Ok(failure(
                self.name(),
                format!(
                    "old_string not found in {path} (0 matches). It must match the file \
                     exactly, including indentation and line breaks — read the file and copy the \
                     text verbatim."
                ),
            ));
        }
        if count > 1 && !replace_all {
            return Ok(failure(
                self.name(),
                format!(
                    "old_string is ambiguous in {path}: {count} matches. Include more \
                     surrounding context so it matches exactly once, or pass replace_all=true \
                     to replace all {count}."
                ),
            ));
        }
        if old == new {
            return Ok(failure(
                self.name(),
                "old_string and new_string are identical; nothing to do.".to_owned(),
            ));
        }

        let (updated, replaced) = if replace_all {
            (document.text.replace(&old, &new), count)
        } else {
            (document.text.replacen(&old, &new, 1), 1)
        };
        let diff = unified_diff_for(&path, &document.text, &updated);
        std::fs::write(&target, document.encode(&updated))
            .map_err(|error| ToolError::io(&error))?;
        let header = format!(
            "Edited {path} ({replaced} replacement{}).",
            plural_s(replaced)
        );
        let body = if diff.is_empty() {
            header
        } else {
            format!("{header}\n{diff}")
        };
        let mut result = success(self.name(), ctx.policy.truncate(body));
        result.metadata.insert("path".to_owned(), json!(path));
        result.metadata.insert("diff".to_owned(), json!(diff));
        result
            .metadata
            .insert("replacements".to_owned(), json!(replaced));
        Ok(result)
    }
}

/// Python's `bool(value)` for a raw preview argument.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

pub struct ListDirTool;

impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the entries of a directory within the workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "path": {
                    "default": ".",
                    "description": "Directory path, relative to the workspace.",
                    "title": "Path",
                    "type": "string",
                },
            },
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let path = arguments.string("path", Some("."));
        arguments.finish()?;

        let target = ctx.policy.resolve_within(&path)?;
        if !target.is_dir() {
            return Ok(failure(self.name(), format!("Not a directory: {path}")));
        }

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&target).map_err(|error| ToolError::io(&error))? {
            let entry = entry.map_err(|error| ToolError::io(&error))?;
            let path = entry.path();
            entries.push(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_file: path.is_file(),
                is_dir: path.is_dir(),
            });
        }
        // Directories (and anything else that is not a regular file) first, then by
        // lowercased name, as Python's `key=lambda p: (p.is_file(), p.name.lower())`.
        entries.sort_by(|a, b| match a.is_file.cmp(&b.is_file) {
            Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            other => other,
        });

        let lines: Vec<String> = entries
            .iter()
            .map(|entry| {
                let marker = if entry.is_dir { "/" } else { "" };
                format!("{}{marker}", entry.name)
            })
            .collect();
        let body = if lines.is_empty() {
            "(empty)".to_owned()
        } else {
            lines.join("\n")
        };
        Ok(success(self.name(), ctx.policy.truncate(body)))
    }
}

struct Entry {
    name: String,
    is_file: bool,
    is_dir: bool,
}
