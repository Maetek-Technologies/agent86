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

/// The current time, for the end of a turn's system message. Beyond Python, whose
/// prompt carries no date: without one, a model cannot tell that a page cached
/// hours ago is stale, nor what "today" is. UTC is given too, since tools often
/// stamp their results in UTC. Fixed for a turn, so its steps share one prompt.
pub fn time_note(now: &jiff::ZonedDateTime) -> String {
    format!(
        "Current date and time: {} (UTC{}), which is {} UTC. Tool results can be cached \
         or out of date: when the question is about the present, check the dates they carry.",
        now.strftime("%A, %Y-%m-%d %H:%M"),
        now.strftime("%:z"),
        now.timestamp().strftime("%Y-%m-%d %H:%M"),
    )
}

/// Where `run_command` and `python_exec` run, for the environment line (beyond
/// Python, whose line names no platform). Without it a model assumes Linux: in
/// the eval set on Windows, two runs in three spent calls on `python3`,
/// `/home/...`, or `/usr/bin`, none of which work there.
pub fn platform_note(sandbox: crate::config::SandboxMode) -> String {
    use crate::config::SandboxMode;
    let (platform, shell, python) = match sandbox {
        SandboxMode::Docker => ("Linux, in a container", "/bin/sh", "python"),
        SandboxMode::Subprocess if cfg!(windows) => ("Windows", "cmd.exe", "python"),
        SandboxMode::Subprocess if cfg!(target_os = "macos") => ("macOS", "/bin/sh", "python3"),
        SandboxMode::Subprocess => ("Linux", "/bin/sh", "python3"),
    };
    format!(
        "Platform: {platform}; run_command runs each command with {shell} in the workspace, and \
         Python is `{python}`."
    )
}

/// Compile the system prompt into a single system message. Beyond Python, the
/// environment line names the platform ([`platform_note`]).
pub fn build_system_prompt(config: &Config, skills: &crate::skills::Skills) -> Message {
    let mut content = format!(
        "{}\n\nEnvironment: agent86 v{}. Default model: {}. {} Sandbox: {}.",
        BASE_IDENTITY.trim(),
        env!("CARGO_PKG_VERSION"),
        config.model.default,
        platform_note(config.sandbox.mode),
        config.sandbox.mode.as_str(),
    );
    content.push_str(&skills_section(skills.values().map(AsRef::as_ref)));
    Message {
        role: Role::System,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    }
}

/// The skills section of the system prompt (empty with no skills): names and
/// descriptions only, since `use_skill` loads the rest. A specialist sub-agent's
/// prompt lists only the skills it may load, with this same text.
pub fn skills_section<'a>(skills: impl Iterator<Item = &'a crate::skills::Skill>) -> String {
    let mut content = String::new();
    for skill in skills {
        if content.is_empty() {
            content.push_str(
                "\n\nAvailable skills (call use_skill with the name to load full instructions):",
            );
        }
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
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_line_gives_local_time_its_offset_and_utc() {
        let eastern = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
        let now = jiff::civil::date(2026, 10, 29)
            .at(21, 5, 0, 0)
            .to_zoned_datetime(eastern)
            .unwrap();
        assert_eq!(
            time_note(&now),
            "Current date and time: Thursday, 2026-10-29 21:05 (UTC-04:00), which is \
             2026-10-30 01:05 UTC. Tool results can be cached or out of date: when the \
             question is about the present, check the dates they carry."
        );
    }

    #[test]
    fn the_platform_note_names_the_shell_and_python_run_command_gets() {
        use crate::config::SandboxMode;
        assert_eq!(
            platform_note(SandboxMode::Docker),
            "Platform: Linux, in a container; run_command runs each command with /bin/sh in \
             the workspace, and Python is `python`."
        );
        let local = platform_note(SandboxMode::Subprocess);
        if cfg!(windows) {
            assert_eq!(
                local,
                "Platform: Windows; run_command runs each command with cmd.exe in the \
                 workspace, and Python is `python`."
            );
        } else {
            assert!(local.contains("/bin/sh"), "{local}");
            assert!(local.ends_with("Python is `python3`."), "{local}");
        }
    }
}
