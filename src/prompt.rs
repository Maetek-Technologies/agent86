//! System prompt compilation, matching the Python `cognitive/prompt.py`.
//!
//! Python appends a skills section when skills are installed, an MCP section
//! when servers are connected, and an episodic-memory recall note when memory
//! has something relevant. None of those subsystems are ported yet, so the
//! prompt here is the identity plus the environment line, which is exactly what
//! Python sends when none of them are active.

use crate::config::Config;
use crate::types::{Message, Role};

/// The harness identity, verbatim from Python's `_BASE_IDENTITY`.
///
/// `tests/fixtures/prompt/base_identity.txt` holds the string extracted from the
/// Python source; a test keeps the two identical.
pub const BASE_IDENTITY: &str = "\
You are agent86, an autonomous agent running inside a deterministic agentic harness.

You operate in a Reason -> Act -> Observe loop. When tools are available, prefer taking a
concrete action over speculating; the harness executes your tool calls, validates them,
and returns real results for you to reason over. When no tool is needed, answer directly
and concisely.

Principles:
- Be truthful about what you did and did not do. Never claim a tool ran if it did not.
- Keep responses focused; avoid filler.
- If a request is ambiguous and the answer would change materially, ask a brief question.
- Use memory sparingly. Only call `remember` for durable, user-specific facts (a stated
  preference, identity detail, or lasting project constraint) — typically when the user
  asks you to remember something. Never remember answers you just computed, transient state,
  or general knowledge. When in doubt, don't remember.

Debugging:
- When code fails, wrap the suspect part in try/except and print(traceback.format_exc())
  instead of guessing at the cause.
- Inspect the actual shape of data before indexing into it — print list(obj.keys()),
  type(obj), len(obj) — rather than assuming a structure.
- When something works on one attempt and fails on another, treat the difference as a clue
  about your own code paths, not as proof the environment is flaky.
";

/// Compile the system prompt into a single system message.
pub fn build_system_prompt(config: &Config, skills: &crate::skills::Skills) -> Message {
    let mut content = format!(
        "{}\n\nEnvironment: agent86 v{}. Default model: {}. Sandbox: {}.",
        BASE_IDENTITY.trim(),
        env!("CARGO_PKG_VERSION"),
        config.model.default,
        config.sandbox.mode.as_str(),
    );
    if !skills.is_empty() {
        // Progressive disclosure: names and descriptions only; `use_skill` loads the rest.
        content.push_str(
            "\n\nAvailable skills (call use_skill with the name to load full instructions):",
        );
        for skill in skills.values() {
            content.push_str(&format!(
                "\n- {}: {}",
                skill.name,
                crate::skills::one_line(&skill.description)
            ));
            // Said up front: finding the restriction through a refused call wastes a step.
            if !skill.allowed_tools.is_empty() {
                content.push_str(&format!(
                    " [tools while active: {}]",
                    skill.allowed_tools.join(", ")
                ));
            }
        }
    }
    Message {
        role: Role::System,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}
