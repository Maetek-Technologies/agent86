# Specialist sub-agents: defined, named, and callable

**Status:** design (not yet implemented; anything marked *proposed* or *open* is unconfirmed)
**Last updated:** 2026-11-08

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

## Decisions (2026-11-08)

- **Fan-out uses one thread per specialty**, not futures on the turn's runtime
  (section 6).
- **The built-in `reviewer` keeps `run_command`, and its commands are always
  asked about**, even in `auto` mode (section 4). Only `researcher` and
  `planner` are read-only.
- **A specialty's system prompt is a filtered copy of the parent's**: it lists
  only the tools and skills that specialty may use (section 2).
- **Sub-agents follow routing.** With no `model:`, a sub-agent runs on the model
  the turn was routed to. This also applies to plain `delegate`, and fixes
  today's mismatch (section 2).
- **`#name` is a hint to the main agent**, not a direct run (section 3).
- **Project specialties are approved per file**, separately from project-file
  trust, and `--trust-project` also covers them (section 4).
- **`--with-context` reuses the compaction summarizer** (section 9).
- **Editors get `delegate`, `/agent`, and `/agents` in the first version**;
  `#name` stays terminal-only (section 10).

## Proposed design

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

Resolution: a defined specialty's body replaces the generic persona; `model`
builds its own provider through `provider_for_ref`; `max_steps` is clamped
under the turn's `steps` as `agent_steps` already is.

**System prompt.** Today a sub-agent gets its persona ahead of the parent's
whole system content (`run_subagent` in `src/subagent.rs`), which would advertise
skills and MCP tools a specialty may not use. A specialty gets its instructions,
then the base identity and environment, then only the tools and skills its
allowlist permits. Plain `delegate` with `role` alone keeps Python's full copy.

**Tool allowlist.** `tools` filters the specs the sub-agent sees and is enforced
in `ToolRegistry::dispatch`, so a refused call is refused even if the model
invents it. It cannot simply reuse the active skill's `allowed-tools`: that slot
holds one skill, the model can replace it by calling `use_skill`, and
`ToolContext::fork` copies the parent's. The specialty's list needs its own
field on the forked context, set once at spawn, and a call must pass both it and
the active skill's list. *Proposed:* a missing `tools` line inherits the parent's
tools except `delegate`, so a specialty can only delegate further if it lists
`delegate`; `max_depth` still applies.

**Model.** With no `model:`, the sub-agent uses the turn's model
(`active_provider()`). Today `run_subagent` reads that provider but sends the
request to `self.provider`, the session default, while its records name the
routed model; providers ignore `CompletionRequest.model`, so the routed name is
discarded. Sending the request to the routed provider fixes both, and differs
from Python, so it is recorded in the plan when it lands.

### 3. How the user calls one

*Decided:* `/agent <name> <task>` runs the specialty directly as a sub-agent, with
no main-agent call, and puts its answer in the conversation. It is the guaranteed
route. `agent86 run --agent <name> <goal>` makes the specialty the *main* agent
of the run (its body replaces the persona, its tools and model apply to the
turn itself). A `/agents` command lists and inspects them.

`#name` is a new sigil handled next to `@path` in `mentions.rs`, and it is a hint
to the main agent: `#reviewer check the auth changes` sends the prompt as typed
plus a note asking the model to delegate to `reviewer`. The main agent writes the
task, so mixed requests and surrounding context work, at the cost of one more
model turn. The model can still decline, which is why `/agent` exists. Several
`#` mentions name several specialties, which the model may fan out (section 6).
An unknown `#name` is left as typed (a `#` in prose, an issue number) rather than
refused. Specialty names must start with a letter, so `#12` is never one.

*Open:* what `run --agent` does with `--model` (the explicit flag wins over the
specialty's `model`, proposed).

### 4. Safety rules that must hold

- A specialty can **narrow** authority, never widen it: its `approval` may be
  stricter than the session's, never looser; its tools are a subset of what the
  parent has.
- A specialty can name tools that are **always asked about**, whatever the
  session's mode: *proposed* frontmatter `confirm: run_command`, implemented
  beside the existing rule in `ApprovalGate::decide` that asks about guarded
  files in every mode but `deny`. `deny` still declines, and with no terminal the
  call is declined. The built-in `reviewer` uses this, because a shell can write
  files.
- Project-level definitions are untrusted input (a cloned repo can ship one),
  and an instruction body is prompt text from the repo. The first use of a
  project specialty asks for approval, showing the file; the approval is
  remembered by the file's content hash, so an edited file asks again. This is
  separate from project-file trust (`src/trust.rs`), which covers only the config
  file. `--trust-project` or `AGENT86_TRUST_PROJECT` also approves project
  specialties, for CI; otherwise, without a terminal, an unapproved project
  specialty is refused, as `ask` mode declines there.
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

- A turn's `delegate` calls that name specialties run concurrently, bounded by
  `[agents] max_parallel` (3; the rest queue). **Each gets a scoped thread with
  its own current-thread runtime.** Futures on the turn's runtime would not be
  enough: runtimes here are single-threaded and tools run synchronously, so one
  long `run_command` would freeze every sibling. `execute_batch` already shares
  `&self` across scoped threads for parallel reads, and the prompt hooks are
  `Send + Sync`, so this follows an existing pattern.
- `delegate` currently falls in the batch's sequential part. Fan-out adds a third
  group: specialty delegations run together, after reads and before writes
  (*proposed*). Plain `role`-only delegations stay sequential, as in Python.
- **Prompts are serialized.** Approval requests from sub-agents pass through one
  queue, and the panel shows which specialty is asking. A declined or pending
  call blocks only its own sub-agent. Today's rule that approvals are settled on
  the loop's thread before a batch runs cannot hold for a call that starts
  mid-sub-agent, so this replaces it for sub-agents. The queue must also carry
  the other prompts a sub-agent can raise: `raise_limit`'s offer and
  `request_secret`.
- Tool results are still pushed back to the parent in call order, whatever
  order the sub-agents finish.
- Usage is summed into the step as now; the cost cap is checked as each
  finishes, and a trip cancels the siblings at their next safe point
  (`CancelToken`, the same safe points as a turn).
- Concurrent writers are the hazard: two specialties editing one file. Two
  mitigations, both used: read-only specialties (`researcher`, `planner`) and the
  always-asking `reviewer` are the common fan-out case, and a specialty that
  writes while others run should use isolation (section 7). The harness refuses
  nothing automatically; the system prompt says to isolate parallel writers.

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
- In an editor session the fork's `ToolContext::editor` must be cleared, or file
  tools would read and write the editor's buffers for the real workspace instead
  of the worktree.

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
- `summary`: text the parent wrote. For `/agent`, `--with-context` produces it by
  running the existing compaction summarizer (`src/memory/working.rs`) over the
  conversation, so its prompt, cost, and routing to the cheap model behave as
  compaction's do.

Everything attached passes the same redaction and guardrails as any other
message. The sub-agent still returns only its final text; its own history is
not given back to the parent.

### 10. Editors (ACP)

In the first version, `agent86 acp` sessions get model delegation to specialties
and `/agent` and `/agents`, added to `COMMANDS` in `src/acp/harness.rs`. Approval
of a project specialty goes through `session/request_permission`. `#name` is
terminal-only for now: editors have no `#` completion, and a mistyped name would
silently stay as plain text. The serialized prompt queue (section 6) must work
for ACP's per-call prompts as well as the terminal panel.

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

Still open (each has a proposed answer in the section named):

1. **`run --agent` with `--model`:** the explicit flag wins over the specialty's
   `model` (section 3).
2. **A specialty with no `tools` line** inherits everything except `delegate`
   (section 2).
3. **The `confirm:` frontmatter key** for always-asked tools (section 4).
4. **Where specialty delegations run in a batch:** after reads, before writes
   (section 6).

## Rough slices (provisional)

0. Fix sub-agent routing: send the request to the routed provider, on its own
   and first, since plain `delegate` changes too.
1. Parse and discover definitions (`src/agents_def.rs`, pure, tested with
   fixtures; no behavior change).
2. `delegate`'s `agent` argument, persona replacement, filtered system prompt,
   system-prompt listing.
3. Tool allowlist (its own context field), always-asked tools, and per-specialty
   step budget.
4. Per-specialty model.
5. Project-specialty trust (approval by content hash, `--trust-project`).
6. Context passing (`files`, `summary` through the compaction summarizer).
7. User entry points (`/agents`, `/agent`, `#name` as a hint, `run --agent`),
   interface display, and the ACP commands and approval.
8. Parallel fan-out on scoped threads with the serialized prompt queue (the
   riskiest slice; needs tests for ordering, cancellation, and approval, limit,
   and secret prompts interleaving, in the terminal and over ACP).
9. Worktree isolation, including clearing the editor hook.
10. Skills both ways.
11. Built-in specialties, embedded and tested for the prompt text and tool lists.
12. Docs, plan entry, README status.

Each slice lands with tests that pin the contract; none needs credentials or
network (the mock server covers delegation already).
