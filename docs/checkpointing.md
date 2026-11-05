# Checkpointing and resume: recovering a turn without starting over

**Status:** design (not yet implemented)
**Last updated:** 2026-11-04

This document records a recommendation for making agent86 recover from a
failure partway through a turn more cheaply and more safely. It came out of an
outside question: *"If each step's output is verified and saved, a failure at
step 14 costs one retry instead of a full restart. Does agent86 resume from the
last verified step, or restart the run?"*

The short answer is that checkpointing makes retries cheap, but only
verification changes the reliability math. Agent steps also change the
workspace, so a checkpoint of the conversation alone is not a full checkpoint.
The recommendation is four tiers, each one usable alone, from nearly free to
expensive. Do them in order and stop wherever the value runs out.

Everything here is **beyond Python** unless noted, so each tier that lands gets
an entry in `docs/PROJECT_PLAN.md` as a deliberate difference. The session
database (`state_json`) stays readable by Python throughout.

## What agent86 does today

agent86 resumes from the conversation, not from a verified step.

- **Saving.** The session is saved to SQLite before and after every turn,
  including failed ones (`src/main.rs`, `persist` around `run_turn_events`; the
  REPL and interface do the same in `Repl::persist`). It is also saved right
  after a compaction (`src/agent.rs`, in `compact_if_needed`), so a resume never
  sees the stale, uncompacted history.
- **A failed turn keeps its progress.** On a provider error, a tripped circuit
  breaker, or a cancel, the steps that finished stay in `state.messages` and
  `state.steps`. The state is left in the `error` phase and saved. `run
  --session <id> <goal>` continues from that history, so the model sees what it
  already did and does not redo it.
- **Most step failures never fail the turn.**
  - A failed tool call goes back to the model as an `ok = false` observation,
    and the model can correct course on the next step.
  - Provider errors are retried inside the step (`src/provider/retry.rs`), but
    never after text has streamed.
  - The `CircuitBreaker` ends runaway turns: too many steps, too much cost, or
    too many errors in a row.
- **The gaps:**
  - **Saved per turn, not per step.** If the process dies mid-turn, the steps
    since the last save are lost.
  - **No automatic resume.** Continuing needs a person and a new user message.
  - **No verification.** `ToolResult.ok` only means the tool did not error, not
    that the step was right.
  - **No workspace checkpoint.** The conversation can be resumed, but the files
    a step changed cannot be rolled back.

## The reasoning

### Checkpointing helps with failures that announce themselves

If any failure means starting over, a 20-step task at 95% per-step success
finishes on the first try 0.95^20 ≈ 36% of the time. That is about 2.8 full runs
on average, roughly 56 steps of work. If each step is checked and saved, the
task takes about 20 / 0.95 ≈ 21 step executions.

That holds for failures that announce themselves: a crash, a timeout, an HTTP
500. For those, checkpointing is close to pure gain.

### Verification decides the reliability

The compounding failures that matter are mostly silent: a plausible wrong edit,
or a wrong conclusion drawn from a file. A checkpoint saves a silent error as if
it were good. Suppose a checker catches 80% of bad steps:

- Each step slips through bad 0.05 × 0.2 = 1% of the time.
- Over 20 steps, 0.99^20 ≈ 82% of runs come out right.

That gain comes from the checker; checkpointing only makes the retry cheap. And
the 80% assumes the checker's errors are independent of the worker's. A model
checking its own work, especially the same model, makes correlated mistakes.
Deterministic checks (tests, builds, schemas) are worth more than model-judged
ones.

### Retrying a step only helps with chance failures

If step 14 fails because the task is beyond the model, retrying it fails again,
more cheaply. Retries need a limit, and then a way out: replan, or ask the user.

### An agent's state includes the workspace

An agent's state is the message log plus the files and whatever its commands
changed. Rolling the conversation back to step 13 does not undo a half-applied
`edit_file` or a `run_command` that appended to a file. A full checkpoint covers
both, and some side effects (network calls, databases, processes, installed
packages) cannot be checkpointed at all.

### ReAct has no fixed steps

The ReAct loop plans as it goes, so there is no predefined "step 14" with
acceptance criteria. The natural checkpoint is the end of each tool batch,
where the loop calls `record_step` (`src/agent.rs`). At that point every tool
call in the history has an answer, which is the invariant a clean resume needs
and the one the cancel path already keeps.

## Recommendation

| Tier | What | Value | Risk | Recommendation |
|---|---|---|---|---|
| 1 | Save after every step | Survives crashes and kills | Low | Do it |
| 2 | Explicit resume of an unfinished turn | Continue without a new message | Low–medium | Do it, with the limits below |
| 3 | Workspace snapshots and rollback | Undo a step's file changes | High (secrets, false safety) | Design first; decide later |
| 4 | Verification hooks | The actual reliability gain | High (code execution, gaming) | Design first; decide later |

### Tier 1: save after every step

**What.** Call `persist` after each `record_step` inside the turn, in addition to
the existing saves before and after the turn.

**How.**

- Save only at step boundaries, never mid-batch, so every tool call in the saved
  history has an answer.
- A failed write must never fail the turn. It loses one checkpoint, as a failed
  episode write loses one episode today.
- The harness already holds the store through `self.memory`, which the
  compaction path uses. The turn's other saves stay where they are.

**Compatibility.** The `state_json` format does not change. What changes is that
a saved state can now be mid-turn: phase `execute`, the last message a tool
result or an assistant message with tool calls, and no final answer yet. Python
must load and resume such a state without errors. Add a fixture of a mid-turn
state and check it against Python's pydantic models, the way
`tests/fixtures/session/python_state.json` covers the other direction. Record
the extra saves in the plan as a difference.

**Costs.** One SQLite write per step on the connection the CLI, harness, and
memory tools share. It is likely negligible next to model latency, but do not
claim that without measuring.

**Tests.**

- A turn killed after step N, then resumed, has steps 1..N in its history.
- A failed write does not change the turn's outcome.
- The mid-turn state fixture loads in both implementations.

### Tier 2: explicit resume

**What.** Continue an unfinished turn without a new user goal, for example `run
--session <id> --resume` and a `/resume` command in the REPL. "Unfinished" means
the last saved state ended in `error` (or in `execute` after a crash) with no
final answer.

**Rules.**

- **The user always starts it.** Never resume automatically. Restarting is
  sometimes the better choice: a turn that failed because it went the wrong way
  (looping, a bad assumption, a context full of failed attempts) keeps going the
  wrong way when resumed. A fresh start gets a clean context.
- **The budget carries over.** The step count, cost, and error count must not
  reset on resume, or "fail, resume, fail, resume" gets around the
  `CircuitBreaker`. Either persist the breaker's counters with the turn, or make
  each resume an explicit choice to spend more, the way the `LimitPrompt` hook
  asks to raise a limit today.
- **Approvals do not carry over.** Every side-effecting call after a resume goes
  through the approval gate again.
- **Tell the model time has passed.** On resume, add a notice that the
  workspace may have changed since the last observation, so the model re-reads
  before trusting old results. Use a notice, not a fake user message, so the
  history still shows what really happened.

**Compatibility.** Python has no resume. A Python user still continues with a
new message, which keeps working.

**Tests.**

- A resumed turn keeps the breaker's counts.
- Resuming a finished turn is refused with a clear message.
- Approvals are asked again after a resume.
- `run --json` output for a resumed turn still has the contract fields.

### Tier 3: workspace snapshots (design before building)

**What.** A snapshot of the workspace after every step that wrote, so the user
can roll the files back to step N. When a resume continues past finished calls,
it uses their recorded results instead of running them again.

**Problems to solve first.**

- **Secrets.** `write_file` and `edit_file` expand `${secret:NAME}` to the real
  value on disk. A snapshot after that step stores a copy of the secret, which
  breaks the vault's rule that values persist only where the user chose.
  Options: refuse to snapshot steps where `expands_secrets` replaced anything,
  redact before storing (but then the restore is wrong), or store snapshots only
  in memory for the session. This is a hard requirement, not polish.
- **False safety.** A rollback restores files only. Network calls, databases,
  processes, packages, Docker state, and anything outside the jail stay as they
  are. The interface must say so every time a rollback is offered, and say
  which steps had side effects it cannot undo, using the tool name: anything
  from `run_command`, `python_exec`, `web_fetch`, or MCP tools.
- **The user's own repo.** A shadow repo must never touch the user's index,
  stash, hooks, or `.git/config`. Use a separate git directory under
  `~/.agent86/`, or a content-addressed store, never the project's `.git`.
- **Concurrent edits.** A rollback must not silently overwrite changes the user
  made after the step. Compare against the snapshot first and stop on a
  conflict.
- **Size and ignored files.** Large repos and ignored directories
  (`node_modules`, `target/`, `.env`) make snapshots slow or incomplete.
  Including `.env` is a secrets problem; leaving it out makes the restore
  partial. A sensible default is to snapshot only the files the agent's own file
  tools wrote. Every one of those paths already goes through
  `SandboxPolicy::resolve_within`, so the jail is the place to record them.
- **Replaying stale results.** Replaying a recorded result is safe for side
  effects but may hand the model something no longer true. Pair it with the
  tier 2 notice.

**Likely shape, if it goes ahead.** Snapshot files only, per step, limited to
paths the file tools wrote; drop secrets; keep them in the session; offer
rollback in the interface only, with a conflict check and a side-effect
warning.

### Tier 4: verification hooks (design before building)

**What.** Checks that run after certain tools, such as `cargo test` after an
edit or a schema check after a write, with the result fed back to the model as
an observation. A failing check means the step gets retried or fixed instead of
being built on.

**Problems to solve first.**

- **Hooks run code.** A project-configured hook runs commands when a tool is
  used. It must need project trust (it can never be in `TRUST_FREE`), should
  probably need its own approval the first time, and must run under the sandbox
  executor with the scrubbed environment like `run_command`. Hook output goes
  through guardrail observation and vault redaction like any tool result,
  because it is a new way for text to reach the model.
- **The agent learns to satisfy the check.** The agent may make the check pass
  instead of solving the problem: editing tests, loosening assertions, skipping
  cases. One mitigation is to treat a write to the check's own files (tests, CI
  config) as a guarded target, the way `Tool::guarded_target` asks about
  agent86's own config and skills in every mode except `deny`.
- **Model-judged checks.** An LLM checker shares the worker's blind spots,
  especially when it is the same model. Prefer deterministic checks. If a model
  is used, use a different one and treat its verdict as advisory.
- **Flaky checks.** A flaky test means retries that cannot succeed. Cap retries
  per check and per turn, count failed checks toward the breaker, and stop to
  ask the user when the cap is hit.
- **Cost and context.** Running checks after every edit adds time and fills the
  context, so compaction happens sooner. Keep only a short summary of a check's
  output in context; the trace keeps the full output.

**Likely shape, if it goes ahead.** A `[verify]` table of tool-to-command rules
that needs project trust, plus per-skill checks in `SKILL.md` frontmatter. Only
deterministic commands at first, run in the sandbox, with capped retries and a
guarded-target rule for test files.

## Open questions

- Should tier 1 be optional (`[session] checkpoint = "step" | "turn"`), or
  always on? Always on is simpler if the cost measures as negligible.
- Where should the breaker's counters live for tier 2: inside `AgentState`
  (which Python must ignore) or in a side table?
- For tier 3, is "files the agent wrote" a narrow enough scope, or should
  `run_command` writes be detected too (by diffing the workspace before and
  after)?
- For tier 4, should a failing check block the final answer, or only add to
  it?

## Suggested order

1. Tier 1 with the mid-turn state fixture.
2. Tier 2, with the breaker's counts carried over and a fresh approval after
   the resume.
3. A design review of tiers 3 and 4 against the vault and project-trust rules
   before any code.
