# Skill commands: run a skill by name, everywhere

**Status:** implemented (2026-11-12). The plan's "Known skill differences from
Python" records what was built; where it differs from this design, the plan wins.
**Last updated:** 2026-11-12

This document plans letting the user run a skill directly, as `/<skill-name> [task]`
or `/skill <name> [task]`, in the interactive interface, the plain loop, and editors
over ACP, with each discovered skill listed in the command menus. It is **beyond
Python**: Python's skills reach the model only through `use_skill`, and `/skills`
only lists them. Each difference is recorded in `docs/PROJECT_PLAN.md` ("Known skill
differences from Python", "Known REPL differences from Python") when a slice lands,
and the README and [ACP.md](ACP.md) are updated.

## Where we are today

- **Discovery.** `skills::discover_skills` finds `SKILL.md` folders in
  `<workspace>/.agent86/skills`, `<workspace>/.claude/skills`, `~/.agent86/skills`,
  and `~/.claude/skills` (first one wins), once per session, in `setup::assemble`.
  They live on `ToolContext::skills`.
- **The model chooses.** `prompt::build_system_prompt` lists each skill's name and
  one-line description. When one applies, the model calls `use_skill`
  (`src/tools/skills.rs`), which returns the instructions plus the bundled resources
  and activates the skill, so its `allowed-tools` restrict the rest of the turn.
  `Harness::run_turn_events` clears the active skill on entry and on every exit
  (`src/agent.rs`).
- **The user can only look.** The terminal's `/skills` (`src/repl.rs`) lists them.
  Editors have no `/skills`; their eight commands are a fixed table in
  `src/acp/harness.rs`.
- **Commands cannot start a turn.** In the terminal, `Repl::handle_command` returns
  an `Action`; only plain text becomes `Action::Turn`, and an unknown slash prints
  `unknown command /x`. Over ACP, `Backend::run_command` answers without the model
  (and runs on the connection's thread, with no sink, so nothing could be approved),
  and a slash it does not claim goes to the model as written.

The user cannot say "use this skill now"; they can only hope the model picks it.

## Goal

- `/<name> [task]` runs a turn with that skill already chosen: its instructions in
  front of the model, its `allowed-tools` in force from the first step.
- `/skill <name> [task]` does the same for any skill, including one whose name is
  shadowed by a built-in command or cannot be a command (see section 2).
- Every surface lists the skills: the interface's `/` menu, `/help` and `/skills` in
  both terminal surfaces, and the editor's command list.
- The terminal and the editor share one harness entry point, so a skill turn behaves
  the same everywhere and is an ordinary turn in the session, the trace, and memory.

## Decisions (2026-11-12)

- **Both terminal surfaces and ACP**, in this feature, not ACP alone.
- **Diverging from Python is fine**; each difference is recorded in the plan.
- **One shared harness entry point**, with thin per-surface layers.

## Proposed design

### 1. The harness: `Harness::run_skill_turn`

One method in `src/agent.rs`, next to `run_turn_events`:

```rust
pub async fn run_skill_turn(
    &self,
    state: &mut AgentState,
    skill: &Arc<Skill>,
    task: &str,          // what the model gets after the skill (may be empty)
    display_text: &str,  // what the user typed, e.g. "/pdf summarize report.pdf"
    on_event: impl FnMut(TurnEvent<'_>),
) -> Result<String, HarnessError>
```

It is `run_turn_events` with two differences, so everything a turn does (clearing
the cancel token, vault redaction of the user's text, ingress and egress
guardrails, routing, compaction, usage into `state.usage` so `/cost` is right,
`turn_start` and `turn_end` in the trace, `last_turn`, the phase, the episode) is
shared rather than repeated. *Proposed:* `run_turn_events` gains a private sibling
that takes `Option<&Arc<Skill>>`, and both public methods call it.

1. **The skill is active from the start.** After the entry `clear_skill()`, the
   skill is activated, so `ToolContext::allowed_tool` enforces its `allowed-tools`
   on the first step. The exit `clear_skill()` is unchanged, so the restriction
   still ends with the turn. The model may switch skills with `use_skill` as today.
2. **The instructions are in the user message.** The model sees:

   ```
   The user ran the skill "<name>". Follow its instructions below for this request.

   <the text use_skill would return>

   Request: <task>
   ```

   With no task the last line reads `Request: (none given; follow the skill's
   instructions, and ask the user if it needs more)`. The middle part is exactly
   what `use_skill` returns (instructions, bundled resources, the tool restriction
   note), so *proposed:* that text moves from `UseSkillTool::execute` into a
   `Skill` method (`load_text()`) that both call. `display_text` is what the trace's
   `turn_start` records and what episodes are keyed by, as for a `@file` turn.

Why a user message, not a synthesized `use_skill` call and result in the history:
a fabricated assistant tool call claims the model did something it did not, has to
carry a valid call id for every provider adapter, and would show in `/resume`
replays and the Ctrl+O view as the model's action. A user message is honest (the
user did choose the skill), needs no adapter changes, and stays in the saved
session, so later turns and `--session` resumes keep the instructions, as a
`use_skill` result does today. `state_json` holds only ordinary messages, so Python
can still read the session.

Unknown names never reach this method; each surface answers them itself.

### 2. Names: which skills get a `/name` command

Skill names are not validated today (they are whatever the frontmatter says), so
not every name can be a command. *Proposed* rules, in one function in
`src/skills.rs` (`command_name(&Skill) -> Option<&str>`), used by every surface:

- A shortcut exists only when the name is 1 to 64 characters of ASCII letters,
  digits, `-`, and `_`, starting with a letter or digit. Matching is exact
  (case-sensitive), as `use_skill` is.
- **Built-in commands win.** A skill named `model`, `clear`, `skills`, `skill`, or
  any other built-in of that surface gets no shortcut there (the terminal has 20
  built-ins, editors 8 plus `skills` and `skill`). `/skills` marks it, and
  `/skill <name>` still runs it.
- `/skill <name> [task]` takes the name up to the first space, so a skill whose
  name holds a space can only be chosen by the model, as today. `/skills` says so.

### 3. The terminal: interface and plain loop

- **A new `Action` variant.** A command handler runs on the interface's drawing
  thread, which also serves the approval panel, so a skill turn cannot run inside
  it. `handle_command` returns `Action::Directed` with the request on
  `CommandResult` (`Action` derives `Copy`, so the payload cannot live in it):

  ```rust
  pub enum DirectedTurn {
      Skill { name: String, task: String },
      // Specialist sub-agents' `/agent` (docs/subagents.md) joins here.
  }
  ```

  This is the same mechanism [subagents.md](subagents.md) proposes for `/agent`
  (section 3 there), designed once so both features use it. The interface runs it
  on its turn worker (`ui::run_turn`), the plain loop where it runs a turn, each
  calling `run_skill_turn`.
- **Matching.** `/skill` is a registry entry. `/<name>` is tried when
  `find_command_for_line` finds no built-in: the first word is looked up in the
  session's skills through `command_name`. Only then is it `unknown command /x`.
- **`@path` in the task** expands as in any turn (`expand_mentions`), and its
  refusals are shown the same way. The typed line is what goes into the prompt
  history (redacted) and `display_text`.
- **The `/` menu** (`src/ui/menu.rs`) lists skills after the built-ins, each with
  its one-line description, read from `repl.harness.context.skills` when the menu
  opens. Names and descriptions come from files, so they go through
  `ui::text::sanitize` before they are drawn, as all such text does.
- **`/help`** gains a "Skills" section (the shortcuts, then a line pointing at
  `/skill <name>`). **`/skills`** keeps its layout and adds the command for each
  (`/pdf`, or `/skill "x y"`-style notes for those without one) and a closing hint
  `run one with /<name> [task]`.
- **Display.** The user's line is echoed as typed. The transcript and Ctrl+O view
  show the typed line, not the expanded instructions, since those are the model's
  input, not the user's words. *Proposed:* the interface shows a dim
  `using skill <name>` notice as the turn starts.

### 4. Editors (ACP)

- **`/skills` and `/skill`** join the editor's command table. `/skills` is a plain
  `run_command` answer (the terminal's list as plain text, through
  `Text::plain_text`). `/skill` cannot be a `run_command` entry, for the reason
  above: it must run through the prompt path, with the sink set, so approvals reach
  the editor and `session/cancel` is read.
- **A second list of commands** that `commands` advertises but `run_command` does
  not claim: `/skill` and every skill shortcut. They fall through to
  `HarnessBackend::prompt`, which recognizes them and calls `run_skill_turn` with
  the session's presenter and sink, exactly as a prompt runs. This is the same
  split subagents.md proposes for `/agent` (section 10 there).
- **Per-session lists.** Skills depend on the session's folder, so
  `Backend::commands` becomes `commands(&self, session_id: &str)`.
  `advertise_commands` already runs per session (after `session/new`,
  `session/load`, and `/resume`), so only the signature and the scripted test
  backend change. Each skill entry carries its sanitized one-line description and
  the hint `task (optional)`.
- **Text from files.** Names come only from `command_name` (so they are plain
  ASCII); descriptions are flattened with `one_line` and stripped of control
  characters before they are sent.
- **Check the spec** (`docs/protocol/v1/` and `schema/v1/schema.json`) for any
  rule on command names or list size before building; advertise nothing that is
  not built.

### 5. `run` (*proposed*, last slice)

`run` has no slash commands, and its stdout and `run --json` are compatibility
contracts, so `/name` text is not interpreted there. *Proposed:* a
`run --skill <name> <goal>` option (top-level placement as `clap` requires) calls
`run_skill_turn`; an unknown name is an `error: ...` with exit 1, and `run --json`
gains no fields. It mirrors subagents.md's `run --agent`. Dropped if the
maintainer prefers to keep `run` unchanged.

## Slices

Each slice passes `cargo test --all-targets`, fmt, and clippy `-D warnings`, and
updates the plan.

1. **Harness.** `Skill::load_text` (with `use_skill` switched to it, its output
   unchanged and pinned by the existing tests), `skills::command_name`, and
   `Harness::run_skill_turn`. Tests: the model's first request holds the wrapper
   and the instructions; the skill's `allowed-tools` refuse another tool on the
   first step and stop applying next turn; `turn_start` records the typed line;
   usage, `last_turn`, the saved session, and the episode are as for a plain turn;
   a vault secret in the task is redacted; `command_name` accepts and refuses the
   right names.
2. **Terminal.** `Action::Directed` and `DirectedTurn`, `/skill`, `/<name>`
   matching, the plain loop and the interface running it, `@path` in the task,
   `/help` and `/skills` changes, the `/` menu entries. Tests: `tests/repl.rs`
   (piped: `/name task` runs a turn against the mock with the instructions in the
   request; `/skill name`; a built-in wins over a same-named skill; an unknown
   `/x` is still `unknown command /x`, with no request); `ui::app` tests for the
   menu entries and sanitized descriptions.
3. **ACP.** `/skills`, `/skill`, shortcuts, per-session `commands`, and the
   prompt-path dispatch. Tests: protocol units with the scripted backend (the
   list per session, a skill command reaching `prompt`); `tests/acp.rs` through
   the binary (a project skill listed in `available_commands_update`, `/name task`
   streaming a turn whose request holds the instructions, approval of a write
   during it reaching the editor, `/skills` answered without the model, an unknown
   `/x` going to the model as today); update the existing commands test's
   expected names and hint count.
4. **`run --skill`** (*proposed*). Tests in `tests/cli_run.rs`: the request holds
   the instructions; an unknown name exits 1 with nothing on stdout.
5. **Docs.** README, [ACP.md](ACP.md) (the command list), the plan's skill, REPL,
   and interface differences, and the plan's next-actions entry checked off.

## Settled questions (2026-11-12, as built)

- **The skill turn's user message is saved expanded**, so the model keeps the
  instructions on later turns and resumes, as a `use_skill` result stays.
- **No new trust gate**: project skills get commands without project trust. They
  are read regardless of trust today and the model can already load them; a
  command only lets the user choose one explicitly.
- **`using skill <name>`** prints in the interface and the plain loop, not over
  ACP, where the editor shows the typed command already.
- **Built-in names**: a skill named like any terminal command gets no command of
  its own on any surface (the editor's commands are a subset of the terminal's),
  so a name means the same everywhere; `/skill` reaches it.
- **`run --skill`** was built, as an option of `run` (after the subcommand, like
  `--json`).
