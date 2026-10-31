# Specialist sub-agents: defined, named, and callable

**Status:** brainstorm (not yet implemented; decisions marked *open* are undecided)
**Last updated:** 2026-10-31

This document is the working plan for letting users (and projects) define
sub-agents with a specialty, such as `reviewer`, `test-writer`, or `researcher`,
and having the main agent, or the user, call them by name. It is **beyond
Python**: Python's `delegate` takes only a free-form role string. Each
difference from Python is to be recorded in `docs/PROJECT_PLAN.md` when a slice
lands.

## Where we are today

`delegate(role, task)` (see `src/tools/delegate.rs` and `src/subagent.rs`) runs
a sub-agent that:

- gets Python's persona (`You are a '<role>' sub-agent ...`) ahead of the
  parent's full system content, and the task as its only message;
- gets the **same tools**, approval gate, guardrails, recorder, and model as
  the parent (a fresh `ToolContext::fork`, so its own active skill);
- runs under `[agents] max_steps` (8) and `[agents] max_depth` (2);
- returns only its final text, with its token usage folded into the parent's.

The `role` is just a string the model invents. Nothing makes a `reviewer`
different from a `coder` except the persona sentence, so there is no way to
give a specialty its own instructions, restrict its tools, or pick a cheaper or
stronger model for it.

## Goal

A *specialty* is a named, reusable sub-agent definition:

- its own **instructions** (the persona and working method),
- optionally a **tool allowlist** (a reviewer that cannot write),
- optionally its own **model**, **step budget**, and **approval mode**,
- discoverable by the model (so it knows when to delegate to it) and callable
  by the user directly.

## Decisions so far (2026-10-31)

- **Format:** Markdown files with frontmatter, like `SKILL.md`. No TOML definitions.
- **Invocation:** all three: the model through `delegate`, the user through
  `/agent <name> <task>` or an `@name` mention, and `run --agent <name> <goal>`
  to make a specialty the main agent of a run.
- **Overridable per specialty:** instructions, tool allowlist, model, step
  budget, approval mode (stricter only). Output cap and memory access stay
  inherited for now.
- **Scope:** agent86's own format only (no `.claude/agents` reader for now), and
  a few **built-in specialties** ship with the binary (`researcher`,
  `reviewer`, `planner`; the exact set and prompts are still to be written).
  A user or project file with the same name overrides a built-in.
- **`#name` expands a specialty** in the prompt (not `@`, which stays file
  paths, so there is no clash). The menu offers `#` completion like `@`.
- **Built-ins:** `researcher`, `reviewer`, `planner` confirmed.
- **Model fallback:** if a specialty's `model` cannot be built (no key, unknown
  provider), the sub-agent runs on the parent's model and the user and trace
  get a notice. It does not fail the call.
- **Project-defined specialties need a one-time approval** on first use (and
  again when the file's content hash changes).
- **Skills and specialties link both ways:** a specialty can preload skills
  (`skills:` in frontmatter), and a skill can name a specialty to hand off to.
- **Parallel fan-out:** several specialties can run at once.
- **Isolation:** a specialty can run in a git worktree copy of the workspace.
- **Context passing:** the parent can attach files and a conversation summary
  to a delegation.

## Proposed design (first draft, to be refined by the open questions below)

### 1. Definition format: one Markdown file per specialty

Modelled on `SKILL.md` (`src/skills.rs`), reusing its frontmatter parser and
discovery roots rather than inventing a new one:

```markdown
---
name: reviewer
description: Reviews a diff or a set of files for bugs and style problems. Use after code changes.
tools: read_file, list_dir, run_command
model: anthropic:claude-sonnet-5-5
max_steps: 12
approval: ask
---

You are a code reviewer. Read the changed files, find correctness bugs first,
style second, and report findings as a ranked list with file:line references.
Never edit files.
```

Locations (later wins, as skills do): built-in, `~/.agent86/agents/<name>.md`,
then `<workspace>/.agent86/agents/<name>.md`. A project file overrides a user
file of the same name.

*Decided:* Markdown only. Built-ins are embedded in the binary and sit lowest
in the precedence order.

### 2. How the model calls one

`delegate` gains an optional `agent` argument naming a defined specialty; with
`role` alone it behaves exactly as today, so the Python schema and every
existing transcript stay valid. The available specialties (name and
description only) are listed in the system prompt, the way skills are, so the
model can choose without loading anything.

Resolution: a defined specialty's body replaces the generic persona; its
`tools` filter the registry the sub-agent sees (the same mechanism as a
skill's `allowed-tools`, enforced in `ToolRegistry::dispatch`, so a refused
call is refused even if the model invents it); `model` builds its own provider
through `provider_for_ref`; `max_steps` is clamped under the turn's `steps`
as `agent_steps` already is.

### 3. How the user calls one

*Decided:* `/agent <name> <task>` and a `#name` mention in the prompt (both
run the specialty as a sub-agent and put its answer in the conversation), and
`agent86 run --agent <name> <goal>`, which makes the specialty the *main* agent
of the run (its body replaces the persona, its tools and model apply to the
turn itself). A `/agents` command lists and inspects them.

`#name` is a new sigil handled next to `@path` in `mentions.rs`: `#reviewer
check the auth changes` expands to a delegation to `reviewer`. An unknown
`#name` is left as typed (a `#` in prose, an issue number) rather than
refused. Several `#` mentions in one prompt fan out (see section 6).

*Open:* what `run --agent` does with `--model` (the explicit flag wins over the
specialty's `model`, proposed).

### 4. Safety rules that must hold

- A specialty can **narrow** authority, never widen it: its `approval` may be
  stricter than the session's, never looser; its tools are a subset of what the
  parent has.
- Project-level definitions are untrusted input (a cloned repo can ship one),
  and an instruction body is prompt text from the repo. The first use of a
  project specialty asks for approval, showing the file; the approval is
  remembered by the file's content hash, so an edited file asks again. Without
  a terminal (piped, CI) an unapproved project specialty is refused, as `ask`
  mode declines there.
- Everything it sees and returns goes through the vault's redaction, the
  guardrails, and the recorder, as sub-agents do now (the trace events keep
  Python's names; add `agent` fields rather than new kinds).
- Depth, step, and output limits keep applying; `max_depth` still bounds
  nesting.

### 5. Interface and trace

The interactive interface shows which specialty is running (the existing
sub-agent notices carry the role; this would carry the name). `spawn` and
`subagent_done` trace events already include `role`; a defined specialty
would set `role` to its name so existing trace readers keep working.

### 6. Parallel fan-out

`delegate` is `parallel_safe = false` today because two nested agent loops
would race for one terminal's approval prompt. Fan-out keeps that rule's
purpose and drops its blunt form:

- A turn's `delegate` calls (and `#` mentions) that name specialties run
  concurrently as futures on the turn's runtime, bounded by
  `[agents] max_parallel` (proposed default 3; the rest queue).
- **Approvals are serialized.** Every sub-agent's approval request goes through
  one queue owned by the loop's thread, and the panel shows which specialty is
  asking. A declined or pending call blocks only its own sub-agent.
- Tool results are still pushed back to the parent in call order, whatever
  order the sub-agents finish.
- Usage is summed into the step as now; the cost cap is checked as each
  finishes, and a trip cancels the siblings at their next safe point
  (`CancelToken`, the same safe points as a turn).
- Concurrent writers are the hazard: two specialties editing one file. Two
  mitigations, both used: read-only specialties (the built-ins) are the common
  fan-out case, and a specialty that writes while others run should use
  isolation (section 7). The harness refuses nothing automatically; the
  system prompt says to isolate parallel writers.

### 7. Isolation: worktree copy

Frontmatter `isolation: worktree` (or the `delegate` argument `isolate: true`)
runs the specialty in a `git worktree` of the workspace in a temporary
directory, jailed there through `SandboxPolicy` instead of the real workspace.

- Requires a git repository; outside one the call fails with a clear message
  (no silent fallback to the shared workspace, since that is what the user
  asked to avoid).
- The sub-agent's result includes the worktree's diff against its base; the
  parent (or user) decides what to apply. Nothing merges automatically.
- Cleanup: the worktree is removed when the sub-agent ends unless it changed
  files, in which case its path and branch are reported so the work is not
  lost. A crash leaves a stale worktree; a startup sweep prunes ours by a
  naming prefix.
- Exec tools run with the worktree as their working directory; secrets and the
  scrubbed environment apply unchanged.

### 8. Skills both ways

- **Specialty to skill:** `skills: a, b` in frontmatter preloads those skills'
  instructions into the sub-agent's system content (the skill's own
  `allowed-tools` still narrows its tools, intersected with the specialty's).
- **Skill to specialty:** a skill's frontmatter may name `agent: reviewer`,
  meaning "run this skill's instructions on that specialty". `use_skill`
  then delegates instead of loading into the current agent, so the skill's
  work happens in a separate context. Without the field a skill behaves as
  today.
- A cycle (a skill naming a specialty that preloads that skill) is legal for
  preloading but is cut for delegation: a specialty started by a skill does not
  re-delegate to the same skill.

### 9. Context passing

`delegate` (and `/agent`) accept optional extras beyond `task`:

- `files`: workspace paths whose contents are read, jailed through
  `resolve_within`, size-capped, and put in the sub-agent's first message.
- `summary`: text the parent wrote (or, for `/agent`, a one-line "include the
  conversation so far" flag that has the parent summarize it).

Everything attached passes the same redaction and guardrails as any other
message. The sub-agent still returns only its final text; its own history is
not given back to the parent.

## Open questions

Settled (2026-10-31):

- **Worktrees see committed work only.** A worktree starts from `HEAD`, so a
  specialty in one does not see the parent's uncommitted edits. This is
  documented in the `isolation` help text and in the result's notice when the
  workspace is dirty.
- **`[agents] max_parallel = 3`**, with no separate per-provider cap for now.
- **Built-in tool lists:** `researcher`: `read_file`, `list_dir`, `web_fetch`,
  `recall`; `reviewer`: `read_file`, `list_dir`, `run_command`; `planner`:
  `read_file`, `list_dir`.
- **Python compatibility:** specialty sessions and traces stay readable by
  Python (`role` is the specialty name, no new event kinds). Worktree paths and
  approvals-by-hash are stored outside `state_json` and the session database,
  in a separate file under `~/.agent86/`.
- **Frontmatter names** (`name`, `description`, `tools`, `model`) stay close to
  Claude Code's, so a `.claude/agents` reader is cheap to add later.

Still open:

1. **Summary generation** for `/agent ... --with-context`: proposed to reuse the
   existing compaction summarizer (`src/memory/working.rs`) instead of a new
   model call by the parent, so the summary's prompt and cost behave like
   compaction's. Not yet confirmed.

## Rough slices (provisional)

1. Parse and discover definitions (`src/agents_def.rs`, pure, tested with
   fixtures; no behavior change).
2. `delegate`'s `agent` argument, persona replacement, system-prompt listing.
3. Tool allowlist and per-specialty step budget.
4. Per-specialty model.
5. Project-specialty trust (approval by content hash).
6. Context passing (`files`, `summary`).
7. User entry points (`/agents`, `/agent`, `#name`, `run --agent`) and
   interface display.
8. Parallel fan-out with the serialized approval queue (the riskiest slice;
   needs tests for ordering, cancellation, and approval interleaving).
9. Worktree isolation.
10. Skills both ways.
11. Built-in specialties, embedded and tested for the prompt text and tool lists.
12. Docs, plan entry, README status.

Each slice lands with tests that pin the contract; none needs credentials or
network (the mock server covers delegation already).
