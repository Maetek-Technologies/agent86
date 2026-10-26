//! Read-only file tools, ported from Python's `tools/builtin/files.py`.
//!
//! Both are jailed to the workspace by [`SandboxPolicy::resolve_within`].
//! `write_file` and `edit_file` are side-effecting and arrive with approvals.

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;

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
