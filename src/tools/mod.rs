//! The tool layer, ported from Python's `tools/base.py`, `tools/registry.py`, and
//! `tools/sandbox/policy.py`.
//!
//! A tool validates the model's arguments before anything runs. Invalid
//! arguments, policy violations, and I/O failures all come back as a
//! [`ToolResult`] with `ok = false` that the model can correct itself from;
//! nothing a tool does can crash the agent loop.
//!
//! Only read-only workspace tools exist so far.

pub mod files;

use crate::types::{ToolCall, ToolResult, ToolSpec};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};

/// Default cap on a tool's output, in characters (Python's `max_output_bytes`).
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 100_000;

/// Why a tool call failed before producing a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// Argument validation failed; each entry is `"<field>: <message>"`.
    InvalidArguments(Vec<String>),
    /// What Python reports as an exception escaping `execute`: `"<Exception>: <message>"`.
    Raised { exception: String, message: String },
}

impl ToolError {
    fn raised(exception: &str, message: impl Into<String>) -> Self {
        Self::Raised {
            exception: exception.to_owned(),
            message: message.into(),
        }
    }

    /// Map an I/O error onto the Python exception class it would raise.
    pub fn io(error: &std::io::Error) -> Self {
        let exception = match error.kind() {
            std::io::ErrorKind::NotFound => "FileNotFoundError",
            std::io::ErrorKind::PermissionDenied => "PermissionError",
            std::io::ErrorKind::IsADirectory => "IsADirectoryError",
            std::io::ErrorKind::NotADirectory => "NotADirectoryError",
            _ => "OSError",
        };
        Self::raised(exception, error.to_string())
    }
}

/// A tool the model can call.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    /// True if invoking the tool causes an external side effect (gated by approvals).
    fn side_effecting(&self) -> bool {
        false
    }

    /// The JSON Schema of the arguments, exactly as Python's pydantic models advertise it.
    fn parameters(&self) -> Value;

    /// What this call would do, shown in the approval prompt before it runs (a
    /// unified diff for a file write). Receives the raw, unvalidated arguments and
    /// must never fail; `None` means the argument summary says it all.
    fn preview(&self, _args: &Map<String, Value>, _ctx: &ToolContext) -> Option<String> {
        None
    }

    /// Run the tool. `ToolResult::call_id` may be left empty; [`Tool::run`] fills it in.
    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError>;

    /// Advertise this tool to the model.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            parameters: self.parameters(),
            side_effecting: self.side_effecting(),
        }
    }

    /// Execute `call`, translating every failure into a structured result.
    fn run(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        let mut result = match self.execute(&call.arguments, ctx) {
            Ok(result) => result,
            Err(ToolError::InvalidArguments(errors)) => failure(
                self.name(),
                format!(
                    "Invalid arguments for '{}': {}",
                    self.name(),
                    errors.join("; ")
                ),
            ),
            Err(ToolError::Raised { exception, message }) => {
                failure(self.name(), format!("{exception}: {message}"))
            }
        };
        if result.call_id.is_empty() {
            result.call_id = call.id.clone();
        }
        if result.name.is_empty() {
            result.name = self.name().to_owned();
        }
        result
    }
}

/// A successful result with `content`.
pub fn success(name: &str, content: String) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        name: name.to_owned(),
        ok: true,
        content,
        error: None,
        metadata: Map::new(),
    }
}

/// A failed result with `error`.
pub fn failure(name: &str, error: String) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        name: name.to_owned(),
        ok: false,
        content: String::new(),
        error: Some(error),
        metadata: Map::new(),
    }
}

/// Validates tool arguments with pydantic's semantics and messages.
///
/// Unknown keys are ignored, as pydantic does by default.
pub struct Arguments<'a> {
    args: &'a Map<String, Value>,
    errors: Vec<String>,
}

impl<'a> Arguments<'a> {
    pub fn new(args: &'a Map<String, Value>) -> Self {
        Self {
            args,
            errors: Vec::new(),
        }
    }

    /// A string field; `default` makes it optional.
    pub fn string(&mut self, field: &str, default: Option<&str>) -> String {
        match (self.args.get(field), default) {
            (Some(Value::String(value)), _) => value.clone(),
            (Some(_), _) => {
                self.errors
                    .push(format!("{field}: Input should be a valid string"));
                String::new()
            }
            (None, Some(default)) => default.to_owned(),
            (None, None) => {
                self.errors.push(format!("{field}: Field required"));
                String::new()
            }
        }
    }

    /// A string field accepted under any of `names` (pydantic's `AliasChoices`);
    /// the first name present wins, and errors name the first choice.
    pub fn string_alias(&mut self, names: &[&str]) -> String {
        match names.iter().find(|name| self.args.contains_key(**name)) {
            Some(name) => self.string(name, None),
            None => {
                self.errors.push(format!("{}: Field required", names[0]));
                String::new()
            }
        }
    }

    /// A boolean field with pydantic's lax parsing: `true`/`false`, `0`/`1`, and
    /// strings such as `"yes"`, `"off"`, or `"t"` in any case.
    pub fn boolean(&mut self, field: &str, default: bool) -> bool {
        const PLAIN: &str = "Input should be a valid boolean";
        const UNINTERPRETABLE: &str = "Input should be a valid boolean, unable to interpret input";
        let parsed = match self.args.get(field) {
            None => return default,
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(Value::Number(number)) => {
                let value = number.as_f64().unwrap_or(f64::NAN);
                if value == 0.0 {
                    Ok(false)
                } else if value == 1.0 {
                    Ok(true)
                } else if number.is_f64() && value.fract() != 0.0 {
                    Err(PLAIN)
                } else {
                    Err(UNINTERPRETABLE)
                }
            }
            Some(Value::String(text)) => match text.to_lowercase().as_str() {
                "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
                "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
                _ => Err(UNINTERPRETABLE),
            },
            Some(_) => Err(PLAIN),
        };
        parsed.unwrap_or_else(|message| {
            self.errors.push(format!("{field}: {message}"));
            default
        })
    }

    /// `Err` with every collected error, or `Ok` when all fields were valid.
    pub fn finish(self) -> Result<(), ToolError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments(self.errors))
        }
    }
}

/// Constraints applied to every tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// The workspace root, fully resolved.
    workspace: PathBuf,
    /// Further roots tools may reach. Python adds skill directories here; none yet.
    allow_paths: Vec<PathBuf>,
    pub max_output_chars: usize,
}

impl SandboxPolicy {
    /// A policy jailed to `workspace`, which must exist.
    pub fn new(workspace: &Path) -> std::io::Result<Self> {
        Ok(Self {
            workspace: std::fs::canonicalize(workspace)?,
            allow_paths: Vec::new(),
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Resolve `path` (relative to the workspace) and enforce the jail.
    ///
    /// Resolution follows symlinks the way Python's `Path.resolve()` does, so a
    /// link inside the workspace that points outside it is refused.
    pub fn resolve_within(&self, path: &str) -> Result<PathBuf, ToolError> {
        let candidate = resolve(&self.workspace.join(path));
        let inside = std::iter::once(&self.workspace)
            .chain(&self.allow_paths)
            .any(|root| candidate.starts_with(root));
        if inside {
            Ok(candidate)
        } else {
            Err(ToolError::raised(
                "PolicyError",
                format!(
                    "Path '{path}' resolves outside the workspace jail ({}).",
                    display_path(&self.workspace)
                ),
            ))
        }
    }

    /// Cap `text` at the output limit, saying how much was dropped.
    pub fn truncate(&self, text: String) -> String {
        let length = text.chars().count();
        if length <= self.max_output_chars {
            return text;
        }
        let kept: String = text.chars().take(self.max_output_chars).collect();
        format!(
            "{kept}\n... [truncated {} bytes]",
            length - self.max_output_chars
        )
    }
}

/// Make `path` absolute and canonical, like Python's non-strict `Path.resolve()`.
///
/// Each existing prefix is canonicalized (following symlinks) before the next
/// component is applied, so `..` acts on the real location; components past the
/// last existing directory are appended lexically.
fn resolve(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => {
                resolved.push(part);
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
        }
    }
    resolved
}

/// A path for messages, without Windows' `\\?\` verbatim prefix.
pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    }
}

/// Ambient state handed to every tool at execution time.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub policy: SandboxPolicy,
}

impl ToolContext {
    /// A context jailed to `workspace`.
    pub fn new(workspace: &Path) -> std::io::Result<Self> {
        Ok(Self {
            policy: SandboxPolicy::new(workspace)?,
        })
    }
}

/// Holds every available tool, exposes their specs, and dispatches calls.
#[derive(Default)]
pub struct ToolRegistry {
    /// In registration order, which is the order specs are advertised in.
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        if self.get(tool.name()).is_some() {
            return Err(format!("Tool '{}' is already registered.", tool.name()));
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Box::as_ref)
    }

    /// Tool names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.iter().map(|tool| tool.name()).collect();
        names.sort_unstable();
        names
    }

    /// Specs in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.get(&call.name) {
            Some(tool) => tool.run(call, ctx),
            None => ToolResult {
                call_id: call.id.clone(),
                ..failure(
                    &call.name,
                    format!(
                        "Unknown tool '{}'. Available: {}.",
                        call.name,
                        self.names().join(", ")
                    ),
                )
            },
        }
    }
}

/// The built-in tools ported so far, in Python's registration order.
///
/// Python also registers `run_command`, `python_exec`, and `web_fetch`.
pub fn builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in [
        Box::new(files::ReadFileTool) as Box<dyn Tool>,
        Box::new(files::WriteFileTool),
        Box::new(files::EditFileTool),
        Box::new(files::ListDirTool),
    ] {
        registry
            .register(tool)
            .expect("built-in tool names are unique");
    }
    registry
}
