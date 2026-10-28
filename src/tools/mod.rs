//! The tool layer, ported from Python's `tools/base.py`, `tools/registry.py`, and
//! `tools/sandbox/policy.py`.
//!
//! A tool validates the model's arguments before anything runs. Invalid
//! arguments, policy violations, and I/O failures all come back as a
//! [`ToolResult`] with `ok = false` that the model can correct itself from;
//! nothing a tool does can crash the agent loop.

pub mod exec;
pub mod files;
pub mod memory;
pub mod sandbox;
pub mod skills;
pub mod web;

use crate::types::{ToolCall, ToolResult, ToolSpec};
use sandbox::Executor;
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

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

    /// What [`Tool::preview`] is written in (`diff`, `bash`, `python`), as Python's
    /// `preview_lexer`, so the interface can colour it.
    fn preview_lexer(&self) -> Option<&'static str> {
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

    /// An optional `dict[str, str]` field (default `{}`), each bad value reported
    /// under its own key, as pydantic does.
    pub fn string_map(&mut self, field: &str) -> Map<String, Value> {
        match self.args.get(field) {
            None => Map::new(),
            Some(Value::Object(map)) => {
                for (key, value) in map {
                    if !value.is_string() {
                        self.errors
                            .push(format!("{field}.{key}: Input should be a valid string"));
                    }
                }
                map.clone()
            }
            Some(_) => {
                self.errors
                    .push(format!("{field}: Input should be a valid dictionary"));
                Map::new()
            }
        }
    }

    /// An `int` field with pydantic's lax parsing: integers, whole floats, booleans
    /// (as 0 and 1), and strings of digits (with a sign, single underscores between
    /// digits, surrounding whitespace, and a `.0` tail). Out-of-range values saturate.
    pub fn integer(&mut self, field: &str, default: i64) -> i64 {
        const PLAIN: &str = "Input should be a valid integer";
        const UNPARSABLE: &str =
            "Input should be a valid integer, unable to parse string as an integer";
        let parsed = match self.args.get(field) {
            None => return default,
            Some(Value::Bool(flag)) => Ok(i64::from(*flag)),
            Some(Value::Number(number)) => match (number.as_i64(), number.as_u64()) {
                (Some(value), _) => Ok(value),
                (None, Some(_)) => Ok(i64::MAX),
                (None, None) => {
                    let value = number.as_f64().unwrap_or(f64::NAN);
                    if value.fract() != 0.0 {
                        Err("Input should be a valid integer, got a number with a fractional part")
                    } else if value.abs() > i64::MAX as f64 {
                        Err("Unable to parse input string as an integer, exceeded maximum size")
                    } else {
                        Ok(value as i64)
                    }
                }
            },
            Some(Value::String(text)) => parse_python_int(text).ok_or(UNPARSABLE),
            Some(_) => Err(PLAIN),
        };
        parsed.unwrap_or_else(|message| {
            self.errors.push(format!("{field}: {message}"));
            default
        })
    }

    /// A required `Literal[...]` string field.
    pub fn choice(&mut self, field: &str, choices: &[&str]) -> String {
        match self.args.get(field) {
            Some(Value::String(value)) if choices.contains(&value.as_str()) => value.clone(),
            None => {
                self.errors.push(format!("{field}: Field required"));
                String::new()
            }
            Some(_) => {
                let quoted: Vec<String> =
                    choices.iter().map(|choice| format!("'{choice}'")).collect();
                let listed = match quoted.split_last() {
                    Some((last, rest)) if !rest.is_empty() => {
                        format!("{} or {last}", rest.join(", "))
                    }
                    _ => quoted.concat(),
                };
                self.errors
                    .push(format!("{field}: Input should be {listed}"));
                String::new()
            }
        }
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

/// A string pydantic's lax mode accepts as an `int`, saturating out of range.
fn parse_python_int(text: &str) -> Option<i64> {
    let text = text.trim();
    // A whole number written with a zero fraction ("7.0", "7.00") counts.
    let text = match text.split_once('.') {
        Some((whole, zeros)) if !zeros.is_empty() && zeros.bytes().all(|b| b == b'0') => whole,
        Some(_) => return None,
        None => text,
    };
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    // Digits with single underscores between them, as Python's `int()`.
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let mut value: i64 = 0;
    for digit in digits.bytes().filter(u8::is_ascii_digit) {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
    }
    Some(if negative {
        value.saturating_neg()
    } else {
        value
    })
}

/// Constraints applied to every tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// The workspace root, fully resolved.
    workspace: PathBuf,
    /// Further roots tools may reach: the skill directories, so a skill's bundled
    /// files can be read. (The jail has no read/write split, as in Python; a write
    /// there still passes the approval gate.)
    allow_paths: Vec<PathBuf>,
    pub max_output_chars: usize,
    /// How long one sandboxed process may run before its tree is killed.
    pub timeout: Duration,
    /// Extra environment variable names forwarded into tool processes.
    pub env_passthrough: Vec<String>,
    /// Whether network access is allowed (`web_fetch`).
    pub network: bool,
}

impl SandboxPolicy {
    /// A policy jailed to `workspace`, which must exist.
    pub fn new(workspace: &Path) -> std::io::Result<Self> {
        Ok(Self {
            workspace: std::fs::canonicalize(workspace)?,
            allow_paths: Vec::new(),
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            timeout: Duration::from_secs(30),
            env_passthrough: Vec::new(),
            network: true,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Let tools also reach `roots` (already resolved), leaving out any inside the
    /// workspace, as Python's `default_policy` does with the skill roots.
    pub fn allow(&mut self, roots: impl IntoIterator<Item = PathBuf>) {
        for root in roots {
            if !root.starts_with(&self.workspace) && !self.allow_paths.contains(&root) {
                self.allow_paths.push(root);
            }
        }
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

    /// Fail unless network access is allowed.
    pub fn require_network(&self) -> Result<(), ToolError> {
        if self.network {
            Ok(())
        } else {
            Err(ToolError::raised(
                "PolicyError",
                "Network access is disabled by the sandbox policy.",
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

/// `path` without Windows' `\\?\` verbatim prefix, for handing to other programs:
/// `cmd.exe` refuses a verbatim working directory and falls back to the Windows
/// directory, so child processes must get the plain form.
pub fn plain_path(path: &Path) -> PathBuf {
    PathBuf::from(display_path(path))
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
    /// Where commands and Python snippets run.
    pub executor: Executor,
    /// `web_fetch` settings.
    pub web: web::WebSettings,
    /// Long-term memory for `remember` and `recall`; `None` when memory is off.
    pub memory: Option<crate::session::SessionStore>,
    /// The discovered skills, for `use_skill`.
    pub skills: Arc<crate::skills::Skills>,
    /// The skill `use_skill` activated this turn, whose `allowed-tools` restrict
    /// what may run. Shared by clones of the context.
    active_skill: Arc<std::sync::Mutex<Option<Arc<crate::skills::Skill>>>>,
}

impl ToolContext {
    /// A context jailed to `workspace`.
    pub fn new(workspace: &Path) -> std::io::Result<Self> {
        Ok(Self {
            policy: SandboxPolicy::new(workspace)?,
            executor: Executor::Subprocess,
            web: web::WebSettings::default(),
            memory: None,
            skills: Arc::default(),
            active_skill: Arc::default(),
        })
    }

    fn active(&self) -> std::sync::MutexGuard<'_, Option<Arc<crate::skills::Skill>>> {
        self.active_skill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Make `skill` the active one, replacing (never stacking on) any other.
    pub fn activate_skill(&self, skill: &Arc<crate::skills::Skill>) {
        *self.active() = Some(Arc::clone(skill));
    }

    /// Lift the active skill's restriction, as the end of a turn does.
    pub fn clear_skill(&self) {
        *self.active() = None;
    }

    pub fn active_skill(&self) -> Option<Arc<crate::skills::Skill>> {
        self.active().clone()
    }

    /// Whether `name` may be called now, given the active skill's `allowed-tools`.
    /// `use_skill` always may: it is how the model switches skills, and a skill
    /// that forgot to list it would otherwise be a one-way door.
    pub fn allowed_tool(&self, name: &str) -> bool {
        match self.active().as_deref() {
            Some(skill) if !skill.allowed_tools.is_empty() => {
                name == "use_skill" || skill.allowed_tools.iter().any(|tool| tool == name)
            }
            _ => true,
        }
    }
}

/// Holds every available tool, exposes their specs, and dispatches calls.
///
/// Cloning shares the same tools: the MCP manager keeps a clone so it can take a
/// dropped server's tools out, and put them back on reconnect, while the agent
/// loop is using the registry, as Python's lock-guarded `ToolRegistry` allows.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    /// In registration order, which is the order specs are advertised in.
    tools: Vec<Arc<dyn Tool>>,
    /// Names dropped by [`ToolRegistry::try_register`] because something already owned them.
    collisions: Vec<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, RegistryInner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RegistryInner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn register(&self, tool: Box<dyn Tool>) -> Result<(), String> {
        let mut inner = self.write();
        if inner.tools.iter().any(|known| known.name() == tool.name()) {
            return Err(format!("Tool '{}' is already registered.", tool.name()));
        }
        inner.tools.push(Arc::from(tool));
        Ok(())
    }

    /// Register `tool` unless its name is taken, recording the clash if it is. The
    /// lenient path for mounting MCP tools, where one collision must not abort the run.
    pub fn try_register(&self, tool: Arc<dyn Tool>) -> bool {
        let mut inner = self.write();
        if inner.tools.iter().any(|known| known.name() == tool.name()) {
            inner.collisions.push(tool.name().to_owned());
            return false;
        }
        inner.tools.push(tool);
        true
    }

    /// Remove `tool` only if it is the object registered under its name, so a dropped
    /// MCP tool that lost a name collision never takes the winner out with it.
    /// Register a shared tool unless its name is taken, without recording a
    /// collision: for a change the user asked for, which reports its own.
    pub fn register_shared(&self, tool: Arc<dyn Tool>) -> bool {
        let mut inner = self.write();
        if inner.tools.iter().any(|known| known.name() == tool.name()) {
            return false;
        }
        inner.tools.push(tool);
        true
    }

    pub fn unregister_tool(&self, tool: &Arc<dyn Tool>) -> bool {
        let mut inner = self.write();
        let before = inner.tools.len();
        inner.tools.retain(|known| !same_tool(known, tool));
        inner.tools.len() != before
    }

    /// Is this exact tool object registered (not merely one with the same name)?
    pub fn contains_tool(&self, tool: &Arc<dyn Tool>) -> bool {
        self.read().tools.iter().any(|known| same_tool(known, tool))
    }

    /// Names [`ToolRegistry::try_register`] refused, in the order they were refused.
    pub fn collisions(&self) -> Vec<String> {
        self.read().collisions.clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.read()
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    /// Tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .read()
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        names.sort_unstable();
        names
    }

    /// Specs in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let tools: Vec<Arc<dyn Tool>> = self.read().tools.clone();
        tools.iter().map(|tool| tool.spec()).collect()
    }

    /// Run `call`. The lock is released before the tool runs, so a long call never
    /// blocks the MCP manager from updating the registry.
    pub fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.get(&call.name) {
            Some(_) if !ctx.allowed_tool(&call.name) => skill_refusal(call, ctx),
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

/// The answer to a call the active skill's `allowed-tools` forbid, naming the
/// skill and its list so the model can re-plan rather than retry.
fn skill_refusal(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let skill = ctx.active_skill();
    let (name, allowed) = skill
        .as_deref()
        .map(|skill| (skill.name.as_str(), skill.allowed_tools.join(", ")))
        .unwrap_or_default();
    ToolResult {
        call_id: call.id.clone(),
        ..failure(
            &call.name,
            format!(
                "Tool '{}' is not permitted while the skill '{name}' is active. That skill's \
                 allowed-tools are: {allowed}. Use one of those, or finish with the skill first.",
                call.name
            ),
        )
    }
}

fn same_tool(a: &Arc<dyn Tool>, b: &Arc<dyn Tool>) -> bool {
    std::ptr::eq(Arc::as_ptr(a).cast::<()>(), Arc::as_ptr(b).cast::<()>())
}

/// Python's built-in tools, plus `remember` and `recall` when memory is on and
/// `use_skill` when a skill was found, in its registration order
/// (`default_registry`).
pub fn default_registry(with_memory: bool, with_skills: bool) -> ToolRegistry {
    let registry = builtin_registry();
    if with_memory {
        for tool in [
            Box::new(memory::RememberTool) as Box<dyn Tool>,
            Box::new(memory::RecallTool),
        ] {
            registry
                .register(tool)
                .expect("memory tool names are unique");
        }
    }
    if with_skills {
        registry
            .register(Box::new(skills::UseSkillTool))
            .expect("use_skill is unique");
    }
    registry
}

/// Python's built-in tools, in its registration order.
pub fn builtin_registry() -> ToolRegistry {
    let registry = ToolRegistry::new();
    for tool in [
        Box::new(files::ReadFileTool) as Box<dyn Tool>,
        Box::new(files::WriteFileTool),
        Box::new(files::EditFileTool),
        Box::new(files::ListDirTool),
        Box::new(exec::RunCommandTool),
        Box::new(exec::PythonExecTool),
        Box::new(web::WebFetchTool),
    ] {
        registry
            .register(tool)
            .expect("built-in tool names are unique");
    }
    registry
}
