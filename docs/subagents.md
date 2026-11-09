# Specialist sub-agents: defined, named, and callable

**Status:** design (not yet implemented; anything marked *proposed* is unconfirmed)
**Last updated:** 2026-11-09

This document is the working plan for letting users (and projects) define
sub-agents with a specialty, such as `reviewer`, `test-writer`, or `researcher`,
and having the main agent, or the user, call them by name. It is **beyond
Python**: Python's `delegate` takes only a free-form role string. Each
difference from Python is to be recorded in `docs/PROJECT_PLAN.md` when a slice
lands.

**Terms.** A *specialty* is a definition: a built-in or one named Markdown file. A
*sub-agent* is one run of an agent loop under a parent, of a specialty or of a
plain `role`. `run --agent` makes a specialty the main agent of a run, so that is
not a sub-agent. The `delegate` argument, `/agent`, and `--agent` are called
`agent` because the caller is naming which agent to run.

## Where we are today

`delegate(role, task)` (see `src/tools/delegate.rs` and `src/subagent.rs`) runs
a sub-agent that:

- gets Python's persona (`You are a '<role>' sub-agent ...`) ahead of the
  parent's full system content, and the task as its only message;
- gets the **same tools**, approval gate, guardrails, and recorder as the parent
  (a fresh `ToolContext::fork`, so its own active skill), and the session's model,
  even when the turn was routed to another (section 2, *Model*);
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
  `/agent <name> <task>` or a `#name` hint in the prompt, and
  `run --agent <name> <goal>` to make a specialty the main agent of a run.
- **Overridable per specialty:** instructions, tool allowlist, model, step
  budget, approval mode (stricter only). There is no frontmatter key for the
  output cap (it follows the model, section 2) or for memory (`remember` and
  `recall` are tools, so the allowlist decides). `confirm`, `skills`, and
  `isolation` came with later decisions; *Reference* lists every key.
- **Scope:** agent86's own format only (no `.claude/agents` reader for now), and
  a few **built-in specialties** ship with the binary (`researcher`,
  `reviewer`, `planner`; their prompts are still to be written).
  A user or project file with the same name overrides a built-in.
- **`#name` names a specialty** in the prompt (not `@`, which stays file
  paths, so there is no clash); what it does is settled below. The menu offers
  `#` completion like `@`.
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

Modelled on `SKILL.md` (`src/skills.rs`), reusing its frontmatter parser and the
shape of its discovery (home passed explicitly, precedence by root) rather than
inventing new ones. The roots are its own, and there is no `.claude` root. A
user-defined specialty looks like this (the built-in `reviewer` is described in
section 4):

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

Locations, lowest precedence first: built-in, `~/.agent86/agents/<name>.md`,
then `<workspace>/.agent86/agents/<name>.md`. A later one wins, so a project file
overrides a user file of the same name, as with skills.

*Decided:* Markdown only. Built-ins are embedded in the binary and sit lowest
in the precedence order.

*Proposed:* a name is an ASCII letter followed by ASCII letters, digits, or hyphens,
at most 64 characters, and the frontmatter `name` must equal the file's stem, so
precedence and `/agents` have one name to go by. A file that breaks either rule is
skipped with a notice. Section 4 relies on this, since a name is the one part of a
project file the model sees before approval.

*Proposed:* parsing fails closed. A `tools:` line that is present but empty means no
tools, so the parser has to tell empty from missing (for a skill, an empty list means
unrestricted). A malformed `approval`, `confirm`, `skills`, `max_steps`, `isolation`,
or `tools` value, or a missing `description`, skips the file with a notice rather than
running it with that key ignored.
A tool name that no tool has is not malformed (section 2). `tools` and
`confirm` accept the spellings `parse_allowed_tools` does. `parse_allowed_tools` cannot
fail, so for `tools`, `confirm`, and `skills` malformed means an empty value followed by
indented lines that are not list items, or an unbalanced `[`; `agents_def.rs` checks the
raw frontmatter for both.

### 2. How the model calls one

`delegate` gains an optional `agent` argument naming a defined specialty; with
`role` alone it stays a generic delegation, and existing transcripts stay valid.
Slices 0 and 0b change the model it runs on and make it cancellable; *Nesting* below
and section 9's `files` and `summary` change it further.
The available specialties (name and description only) are listed in the system
prompt, the way skills are, so the model can choose without loading anything. An
unapproved project specialty is listed by name only (section 4).

The new arguments (`agent` here, and `isolate`, `skill`, `files`, and `summary` in
sections 7 to 9) change the tool's schema, a deliberate difference from Python to
record in the plan. `tests/delegation.rs` pins `parameters()` to Python's, so that
test changes to compare Python's `role` and `task` and pin the additions
separately. The description stays Python's, since the specialties are listed in
the system prompt.

Resolution: a defined specialty's body replaces the generic persona; `model`
selects its own model (see *Model* below); `max_steps` is clamped under the
turn's `steps` as `agent_steps` already is. *Proposed:* an unknown `agent` is
answered as a failed call naming the specialties available, with no sub-agent
run. If a call gives both `agent` and `role`, `agent` decides. *Proposed:* `files`
and `summary` also work with `role` alone, since they only add to the first
message; `isolate` and `skill` need `agent`, and without it the call fails saying
so.

**System prompt.** Today a sub-agent gets its persona ahead of the parent's
whole system content (`run_subagent` in `src/subagent.rs`), which would advertise
skills and MCP tools a specialty may not use. A specialty gets its instructions,
then the base identity and environment, then only the tools and skills its
allowlist permits. *Proposed:* skills are listed only when `use_skill` is permitted,
and a specialty permitted `delegate` also gets the specialty listing. The names of
the user's secrets and the time note stay, because `${secret:NAME}` depends on the
first and neither widens anything. The filter applies whenever the context carries a
list, so a `role`-only child of a narrowed specialty does not advertise what it may
not use. Plain `delegate` from the unrestricted main context keeps Python's full copy.

*Proposed:* this needs four changes. `self.system` is one compiled string with the
skills inlined, so the harness stores the parts. `McpManager::prompt_section` takes a
filter, since today it emits every server's instructions, including servers with no
permitted tools. The specialty listing is built per request in `system_message`,
because an approval changes it mid-session. And `run_subagent`'s context budget, which
today counts the parent's system message and every tool, counts the filtered prompt
and specs, including the specialty's body and preloaded skills, which can be long.

**Tool allowlist.** `tools` filters the specs the sub-agent sees and is enforced
in `ToolRegistry::dispatch`, so a refused call is refused even if the model
invents it. It cannot simply reuse the active skill's `allowed-tools`: that slot
holds one skill, the model can replace it by calling `use_skill`, and
`ToolContext::fork` copies the parent's. The specialty's list needs its own
field on the forked context, set once at spawn, and a call must pass both it and
the active skill's list. *Proposed:* the check is in `ToolContext::allowed_tool`, so
`Harness::delegation`, which intercepts `delegate` before dispatch, honours it too,
with its own refusal text, since `skill_refusal` would name a skill that is not there.
`use_skill` is not exempt from this list, though it is from the active skill's.
*Proposed:* `ApprovalGate::decide`, which is given the
context, refuses a call outside the specialty's list before anything else,
including its early return for read-only tools, so nobody is asked about a call
that would then be refused. A name on the list that no tool has is ignored rather
than an error, because `recall` is absent with memory off and MCP tools come and
go during a run. *Decided:* a missing `tools` line inherits the parent's tools
except `delegate`, so a specialty can only delegate further if it lists
`delegate`; `max_depth` still applies.

**Nesting.** *Proposed:* a sub-agent forks the context of whoever delegated, not
the harness's. Today `run_subagent` forks `self.context` and `delegate` is not
given the caller's, so a specialty that lists `delegate` could start a plain
`role` delegation with every tool, and a child of a worktree specialty would run
in the real workspace. A child's list is its own intersected with the caller's,
its approval is the stricter of the two, and `confirm` is the union of both; one
with no `tools` line takes the caller's list minus `delegate`. Plain nesting
changes slightly too, since a child now starts with its caller's active skill
rather than the main loop's, so record that in the plan.

**Approval mode.** *Proposed:* `approval` is a second field on the forked context,
set at spawn. `ApprovalGate::decide` uses the stricter of it and the session's
mode (`deny`, then `ask`, then `auto`), read for each call, so a switch mid-turn
still applies. A looser value has no effect: `approval: auto` in an `ask` session
still asks. `run --agent` sets this field and `confirm` on the run's context.

**Model.** With no `model:`, the sub-agent uses the turn's model
(`active_provider()`). Today `run_subagent` reads that provider but sends the
request to `self.provider`, the session default, while its records name the
routed model; providers ignore `CompletionRequest.model`, so the routed name is
discarded. Sending the request to the routed provider fixes both. It also restores
Python's behaviour: Python reassigns `self.provider` for each turn's route and its
sub-agent uses `h.provider`, so today's code is the difference.

*Proposed:* a specialty's own `model` is built the way a route is, as a
`RoutedModel` (`setup::build_router`), so the sub-agent also has that model's
context window, output cap, and setting name. `run_subagent` reads these from the
model it runs on. Today `context_budget` and `output_cap` read the active model,
which could overflow a smaller one or ask it for more output than it allows.
`/limits output` still replaces the cap for every model, as it does now. If the
model cannot be built, the sub-agent runs on the parent's model with the parent's
limits, and the user and trace get a notice. *Proposed:* the trace's notice is a
`route` event on the sub-agent's session, with `agent` and the model it wanted
beside `model`, so the rule against new event kinds (section 4) holds. The user's
notice goes on a per-turn list on the harness, which the loop drains after the batch
and emits as `TurnEvent::Notice`, so `run`'s stdout stays as it is; `run_subagent`
has no other path to the user. `/agent` emits its own, and slice 7 moves it to a card
line for an editor (section 10).

**Step budget.** *Proposed:* `/limits` or `--max-agent-steps` comes first, then
`max_steps:`, then `[agents] max_steps`, always under the turn's `steps`, matching how
`/limits output` replaces the cap for every model. `raise_limit` stores every raise as
the session's override today, which would hand one specialty's budget to every
sub-agent after it, so it gains a scope. A raise offered to a specialty's run applies
to that run only: its steps and, when the specialty has its own `model:`, its output
cap. A raise offered to a plain delegation stays session-wide as today but ranks with
`[agents] max_steps`, below `max_steps:`, so that raising a plain sub-agent does not
change a specialty that sets its own. The offers' "for this session" wording
(`limits.rs`, `ui/app.rs`) changes for the run-only case.
Under `run --agent`, `max_steps:` can lower the turn's own budget but not raise it, and
`run --max-steps` wins over it, as `--model` does over `model`.

### 3. How the user calls one

*Decided:* `/agent <name> <task>` runs the specialty directly as a sub-agent, with
no main-agent call, and puts its answer in the conversation. It is the guaranteed
route. `agent86 run --agent <name> <goal>` makes the specialty the *main* agent
of the run (its body goes ahead of the base identity, in section 2's order, and its
tools and model apply to the turn itself). A `/agents` command lists and inspects
them.

*Proposed:* the full forms are `/agent [--with-context] <name> <task>` and
`/agents [name]`, which inspects one. In the terminal, `@path` in the task expands as
it does in a turn, and that is `/agent`'s form of section 9's `files`; there is no
separate syntax. Editors have `--with-context` only. The run is at depth 1, like a
delegation from the main loop.

`/agent` cannot be an ordinary command in the terminal, for the same reason as in
an editor (section 10). A command handler runs on the interface's drawing thread,
which also serves the approval panel, so an approval could never be answered and
Escape would not be read. *Proposed:* the handler checks the name and returns a new
`Action` variant, with the specialty and the task on `CommandResult`, since `Action`
derives `Copy`. The interface runs that on
its turn worker (`ui::run_turn`), and the plain loop runs it where it runs a turn.

`#name` is a new sigil handled next to `@path` in `mentions.rs`, and it is a hint
to the main agent: `#reviewer check the auth changes` sends the prompt as typed
plus a note asking the model to delegate to `reviewer`. The main agent writes the
task, so mixed requests and surrounding context work, at the cost of one more
model turn. The model can still decline, which is why `/agent` exists. Several
`#` mentions name several specialties, which the model may fan out (section 6).
An unknown `#name` is left as typed (a `#` in prose, an issue number) rather than
refused. Specialty names must start with a letter, so `#12` is never one.
*Proposed:* this is the REPL and the interface only, since `run` does not expand
mentions. Only the typed line is scanned, before `@` expansion, so a `#x` inside an
attached file cannot inject a hint. A mention is `#` at the start of the line or after
whitespace, ended by whitespace or punctuation, so `foo#x` is not one.

*Decided (2026-11-09):* with `run --agent`, an explicit `--model` wins over the
specialty's `model`, and whichever of them applies bypasses triage routing for the
run. Without `--agent`, `--model` still sets the default and leaves routing on.
With neither a flag nor a `model:`, the turn is routed as usual. `/agent` with no
`model:` routes on its task as a turn does, after the ingress scan, and that route
stays current afterwards; with a `model:` it leaves routing alone. *Proposed:*
`run --agent` bypasses routing the way `/model` does, with `Router::pin`, so
compaction summaries then go to the run's model too.

**`run --agent` in full.** *Proposed:* the specialty is resolved before `assemble`,
and the default model is set from it, because `assemble` builds `config.model.default`
first and exits 1 if that has no key, even when the specialty's `model:` is usable.
`main` records `cli.model.is_some()` first, since `cli.model` moves into `Overrides`.
The order is `--model`, then `model:`, then `AGENT86_MODEL` and the config. A `model:`
that cannot be built fails the run, as an unbuildable route does: there is no parent
to fall back to. The rule for a missing `tools` line is the same, so such a run has no
`delegate`, and the main loop's specs are filtered as a sub-agent's are. `skills:`
preload as in section 8. *Decided (2026-11-09):* `isolation: worktree` is refused,
because nobody is there to take the diff and section 7 rules out a silent fallback.
*Proposed:* `--session` is allowed, and
the specialty applies to that turn. An unknown or refused name prints `error: ...` on
stderr, exits 1, and writes nothing to stdout, including with `--json`.

### 4. Safety rules that must hold

- A specialty can **narrow** authority, never widen it: its `approval` may be
  stricter than the session's, never looser; its tools are a subset of what the
  parent has. This holds through nested delegation (section 2, *Nesting*).
- A specialty can name tools that are **always asked about**, whatever the
  session's mode: frontmatter `confirm: run_command`, implemented in
  `ApprovalGate::decide` like the existing rule that asks about guarded files in
  every mode but `deny`, but ahead of the early return for read-only tools that
  rule sits behind, so `confirm: web_fetch` asks too. `deny` still declines, and
  with nobody to ask (no terminal and no editor) the call is declined. The built-in `reviewer` uses
  this, because a shell can write files.
- The built-ins' tools (settled 2026-10-31): `researcher` has `read_file`, `list_dir`,
  `web_fetch`, and `recall`; `reviewer` has `read_file`, `list_dir`, and `run_command`,
  with `confirm: run_command`; `planner` has `read_file` and `list_dir`.
- Project-level definitions are untrusted input (a cloned repo can ship one),
  and an instruction body is prompt text from the repo. The first use of a
  project specialty asks for approval, showing the file; the approval is
  remembered by the file's content hash, so an edited file asks again. This is
  separate from project-file trust (`src/trust.rs`), which covers only the config
  file. `--trust-project` or `AGENT86_TRUST_PROJECT` also approves project
  specialties, for CI; otherwise, with nobody to ask (no terminal and no editor),
  an unapproved project specialty is refused, as `ask` mode declines there. An
  editor session asks through the editor (section 10).
  - *Proposed:* the whole file is shown on every surface, paged at a terminal, through
    a preview flag that skips `cap_lines` and the panel's cap. Otherwise everything
    past the fortieth line would run unseen.
  - *Decided (2026-11-09):* the flag and the variable do not approve anything in an
    ACP session, where the editor still asks: `docs/ACP.md` tells editor users to add
    `--trust-project` for config trust, which is a different question. *Proposed:* the
    forced value is carried on its own, because `ProjectTrust::Trusted` also means remembered
    or nothing to decide. Approvals live in `~/.agent86/approved-agents.json`, keyed by
    the resolved file path plus SHA-256. Tests of a refusal must `env_remove` the
    variable, because the CLI, REPL, and ACP test harnesses set it.
  - *Decided (2026-11-09):* a refused or declined project file does not fall back to
    the layer beneath it, since that would run something other than what was asked; the refusal
    names the file. So a repo's unapproved `reviewer` shadows the built-in until it is
    approved or removed.
  - *Decided:* until a project specialty is approved, the model sees only its
    name in the listing, marked as needing approval. Its description is repo text
    too, so it joins the listing once the file's current hash is approved, and an
    edited file goes back to name only.
  - *Proposed:* the check lives in one function that every start goes through:
    `delegate` from the main loop, `delegate` from a sub-agent (whose calls do not
    pass through `resolve_call`), `/agent`, and `run --agent`. It asks in every
    mode but `deny`, where an unapproved file is refused without asking, as a
    guarded file is.
- Everything a specialty sees and returns must go through the vault's redaction,
  the guardrails, and the recorder. The trace events keep Python's names, with no new
  kinds: `role` and the existing `agent` field carry the specialty's name, only the
  fallback's `route` gains `agent`, and sub-agent events gain `run` (section 6).
  Today that holds for a sub-agent's tool
  results and its trace events, but its task gets no ingress scan or vault
  redaction. Anything new that reaches a sub-agent from outside a tool call, such
  as section 9's files, has to apply these itself. So does anything new a
  sub-agent hands back that no tool produced, such as section 7's diff, because a
  `delegate` result does not pass through `ToolRegistry::dispatch`.
- Depth, step, and output limits keep applying; `max_depth` still bounds
  nesting.

### 5. Interface and trace

The interactive interface shows which specialty is running. Today it has only the
`delegate` tool call, with the role in its arguments, and the offer to raise a
spent step budget; live progress comes from the observer (section 10). `spawn` and
`subagent_done` trace events already include `role`; a defined specialty
would set `role` to its name so existing trace readers keep working.

### 6. Parallel fan-out

`delegate` is `parallel_safe = false` today because two nested agent loops
would race for one terminal's approval prompt. Fan-out keeps that rule's
purpose and drops its blunt form:

- Adjacent `delegate` calls that name specialties run concurrently, bounded by
  `[agents] max_parallel` (3; the rest queue). **Each gets a scoped thread with
  its own current-thread runtime.** Futures on the turn's runtime would not be
  enough: runtimes here are single-threaded and tools run synchronously, so one
  long `run_command` would freeze every sibling. `execute_batch` already shares
  `&self` across scoped threads for parallel reads, and the prompt hooks are
  `Send + Sync`, so the harness can be shared this way. Only the main loop fans out:
  a sub-agent's own `delegate` calls stay sequential, as in `run_subagent` today.
  *Decided (2026-11-09):* for `max_parallel`, a value below 1 counts as 1 and the
  ceiling is 8.
  The setting is trust-free through the `agents` prefix in `TRUST_FREE`, so a cloned
  repo can set it, which is why it has a ceiling.
- **Each thread needs its own HTTP client.** The parallel reads never make model
  calls; sub-agents do. A provider holds one pooled `reqwest::Client`, and a pooled
  connection only makes progress while the runtime that opened it is being polled.
  Tested 2026-11-09 (reqwest 0.13.5, HTTP/1.1): a child's first call hung when it
  reused the connection left by the parent's model call, whose runtime was blocked
  waiting on the scope, and again when it reused one from a sibling sitting in a
  synchronous tool. With idle pooling off, both worked. *Proposed:* `Provider`
  gains a method returning a copy with the same settings and key and a new client,
  which needs no keyring read. Building a client can fail, and that fails that
  delegation; it never falls back to sharing a client.
  The loop's thread makes one per sibling before
  spawning, and a sub-agent on that thread uses that copy, as does anything it
  delegates to on the same model. A specialty on its own `model` resolves it as
  section 2 says, but the provider that comes back, which may be the `Router`'s
  cached one, is only the source of a copy: what a sub-agent calls on a fan-out
  thread is always a copy made for that thread. The mock server closes every
  connection, so the regression test needs a keep-alive reply in
  `tests/support/mod.rs`.
- *Decided (2026-11-09):* the order the model asked for holds. Reads still go
  first; the calls that remain run in call order, and a run of adjacent specialty
  delegations goes together. So `[write_file, delegate reviewer]` reviews the
  written file, and a write or a plain `role`-only delegation (sequential, as in
  Python) between two delegations separates them.
- *Decided (2026-11-09):* `execute_batch` has two paths. Today a batch with fewer than two
  parallel-eligible calls takes the strictly sequential one, and `delegate` is not
  eligible, so reads would not go first there: `[delegate a, read, delegate b]` would
  run in that order, while `[delegate a, read, read, delegate b]` would group a and b.
  So a batch containing any specialty delegation takes the reads-first path when
  `[limits] parallel_tools` is on; a batch with none is unchanged, as in Python.
  Adjacency is over the pending non-read calls, in call order: a declined call has
  already been answered and is no barrier. With `parallel_tools = false`, nothing is
  grouped and delegations run one at a time.
- **Prompts are serialized.** Approval requests from sub-agents pass through one
  queue, and the panel shows which specialty is asking. A declined or pending
  call blocks only its own sub-agent. Today's rule that approvals are settled on
  the loop's thread before a batch runs cannot hold for a call that starts
  mid-sub-agent, so this replaces it for sub-agents. The queue must also carry
  the other prompts a sub-agent can raise: `raise_limit`'s offer and
  `request_secret`. *Proposed:* it is one `PromptQueue` on the harness, in front of
  all three hooks. After acquiring it, a waiter checks the turn's token and its run's
  and declines without asking, on every surface, or a cancel would be followed by each
  queued question in turn. On a cost-cap trip an open prompt stays until it is
  answered; only queued ones decline.
- Tool results are still pushed back to the parent in call order, whatever
  order the sub-agents finish.
- *Proposed:* concurrent runs of one specialty share the `sub:<role>` session in the
  trace, so sub-agent events gain a `run` field, the per-spawn id of section 10. It is
  additive, and Python's readers ignore it.
- Usage is summed into the step as now. *Proposed:* a fan-out has one per-turn spend
  accumulator, seeded with what the turn has spent so far, fed by each sibling after
  every completion, and checked before each sibling's model call against
  `max_cost_usd`. The overshoot is at most the calls already in flight. A failed run
  must return what it spent, which a provider failure discards today. A trip cancels
  the siblings at their next safe point. A plain sequential `delegate` keeps its own
  breaker, as in Python. Wall-clock limits stay per run, and the parent's is checked
  after the batch. Sub-agents have no safe points yet (slice 0b), and a trip uses each
  run's own token rather than the turn's `CancelToken` (section 10, *Cancellation*).
- *Proposed:* a cancelled or tripped run returns its partial usage and a halted-style
  result that does not count as a tool error; otherwise `max_consecutive_errors` could
  end the loop mid-batch with later calls unanswered, which providers reject. A call
  not started is answered `Not executed: cancelled`, and a thread that panics is
  answered as the read path answers one. `Not executed` answers have `ok = false` and
  count too, so the main loop answers every call in the batch before it returns a trip
  from `record_tool_result`.
- Concurrent writers are the hazard: two specialties editing one file. Two
  mitigations, both used: read-only specialties (`researcher`, `planner`) and the
  always-asking `reviewer` are the common fan-out case, and a specialty that
  writes while others run should use isolation (section 7). The harness refuses
  nothing automatically; the system prompt says to isolate parallel writers (that
  text lands with isolation, in slice 9).

### 7. Isolation: worktree copy

Frontmatter `isolation: worktree` (or the `delegate` argument `isolate: true`)
runs the specialty in a `git worktree` of the workspace in a temporary
directory, jailed there through `SandboxPolicy` instead of the real workspace.

- Requires a git repository; outside one the call fails with a clear message
  (no silent fallback to the shared workspace, since that is what the user
  asked to avoid).
- A worktree starts from `HEAD`, so a specialty in one does not see the parent's
  uncommitted edits. The `isolate` argument's description says so, and the result carries a
  notice when the workspace is dirty.
- *Proposed:* `SandboxPolicy` gains a re-root method, since `new` loses the guards,
  timeout, and passthrough. A project skill's root inside the real workspace is allowed
  explicitly, because `allow` drops roots inside the workspace, so its resources stay
  readable at their real path.
- The sub-agent's result includes the worktree's diff against its base; the
  parent (or user) decides what to apply. Nothing merges automatically. The
  harness builds that diff, so it needs the vault's redaction applied explicitly:
  `write_file` expands `${secret:NAME}`, so the diff can hold the real value
  (section 4). *Proposed:* it is size-capped, saying so and where the rest is,
  since a worktree with changes is kept. Untracked files reach it through `git add -N`.
- *Proposed:* agent86's own `git` calls on the host use an explicit `--git-dir`,
  `-c core.hooksPath=<an empty directory>`, `-c core.fsmonitor=false`, and, for the
  diff, `--no-ext-diff --no-textconv`. Sandboxed code can rewrite a worktree's `.git`
  file to point at a directory holding a crafted config, and a command in the real
  workspace can plant a `post-checkout` hook that `git worktree add` would run. This
  rests on how git behaves and has not been tested here; slice 9 tests it.
- *Proposed:* MCP tools do not go through `SandboxPolicy`, so they act outside the
  worktree. An isolated specialty does not inherit them; it can use one only by naming
  it in `tools`, and the `isolate` argument's description says what that means.
- Cleanup: the worktree is removed when the sub-agent ends unless it changed
  files, in which case its path and branch are reported so the work is not
  lost. A crash leaves a stale worktree; a startup sweep prunes ours by a
  naming prefix, and only those whose owning process has exited, since another
  terminal or editor window may be using one (section 10).
- Exec tools run with the worktree as their working directory; secrets and the
  scrubbed environment apply unchanged.
- *Proposed:* a delegation from inside a worktree stays in that worktree (section
  2, *Nesting*), and one asking for isolation there is served by the same
  worktree rather than a second.
- *Proposed:* under the Docker sandbox, `git` does not work inside an isolated
  specialty's commands, and the `isolate` argument's description says so. A worktree's `.git`
  points at the real repository, which is outside the one directory mounted, and
  mounting it would give the container write access to that repository. The result
  diff is made on the host, so a failing `git` in the container does not affect it.
- In an editor session the fork's `ToolContext::editor` must be cleared, or file
  tools would read and write the editor's buffers for the real workspace instead
  of the worktree.

### 8. Skills both ways

- **Specialty to skill:** `skills: a, b` in frontmatter preloads those skills'
  instructions into the sub-agent's system content. Preloading does not use the
  active-skill slot, which holds one skill. *Proposed:* the tools allowed are
  worked out at spawn and stored in the specialty's own list (section 2): a tool
  must be in the specialty's list and in at least one preloaded skill's
  `allowed-tools`, and a preloaded skill with no `allowed-tools` lifts that second
  condition. With one skill this is a plain intersection. A name in `skills:` that no
  skill has is ignored with a notice.
- **Skill to specialty:** a skill's frontmatter may name `agent: reviewer`,
  meaning "run this skill's instructions on that specialty", so the skill's work
  happens in a separate context. *Decided:* `use_skill` does not start the
  sub-agent itself. It is a synchronous tool that can run in the parallel read
  path, and its one argument, `name`, is pinned to Python's schema, so it could
  neither wait for a sub-agent nor give it a task. It answers instead with a note
  that the skill runs on `reviewer`, asking the model to call `delegate` with
  `agent: "reviewer"`, `skill: "<name>"`, and the task. `delegate`'s `skill`
  argument preloads that skill for this run, as `skills:` does, and the delegation
  passes the usual approval check. The cost is one more model turn. Without the
  field a skill behaves as today. *Proposed:* `use_skill` loads the skill as usual,
  in place of the note, whenever the hand-off could not be made: `delegate` is not
  on the specialty's list, the active skill refuses it, delegation is off, the
  depth limit is reached, or the named specialty does not exist. `use_skill` sees only
  `ToolContext`, so to tell, the context carries the depth, `[agents] enabled` and
  `max_depth`, and the set of specialties.
- A cycle (a skill naming a specialty that preloads that skill) is legal for
  preloading but is cut for delegation: inside a specialty that already has the
  skill preloaded, `use_skill` loads it as usual instead of handing off.

### 9. Context passing

`delegate` accepts optional extras beyond `task`, and `/agent` has its own forms of
them (section 3):

- `files`: workspace paths whose contents are read, jailed through
  `resolve_within`, size-capped, and put in the sub-agent's first message.
  *Proposed:* in an editor session they are read through `ToolContext::editor`, as
  `read_file` is, so an unsaved buffer is what gets attached. `delegate` is not
  side-effecting, so nothing else would check this read: `read_file` must pass the
  caller's list and active skill, and goes through `confirm`. Paths resolve against
  the caller's context, also when `isolate` is set, so what is attached is the
  parent's view, with unsaved buffers and uncommitted edits.
- `summary`: text the parent wrote. For `/agent`, `--with-context` produces it by
  running the existing compaction summarizer over the conversation, so its prompt,
  cost, and routing to the cheap model behave as compaction's do. The prompt is in
  `src/memory/working.rs`, but the call is inside `compact_if_needed`, which also
  rewrites the history, so the request has to be extracted for both to use.

A sub-agent's first message does not pass through `ToolRegistry::dispatch`, so
attached files and summaries need the vault's redaction and the observation
guardrail applied explicitly when the message is built (section 4). The sub-agent
still returns only its final text; its own history is not given back to the parent.

### 10. Editors (ACP)

In the first version, `agent86 acp` sessions get model delegation to specialties
and `/agent` and `/agents`. Approval of a project specialty goes through
`session/request_permission`. `#name` is terminal-only for now: editors have no
`#` completion, and a mistyped name would silently stay as plain text.

`delegate` already runs in an editor today, as one opaque tool call: the editor
sees the call, then nothing until the sub-agent's final text. The changes below
are what specialties need on top of that. Everything marked *proposed* is
unconfirmed.

**What the protocol offers** (checked against the spec repository at `75438f7`,
2026-11-09, unchanged in docs and schema since `a31ae0d`).
Stable ACP v1 has no way to say that a tool call or a message came
from a sub-agent. The [Subagent Sessions RFD](https://agentclientprotocol.com/rfds/subagents)
adds one: each sub-agent becomes a child session announced with
`subagent_update`. It is still an active RFD, exists only in the unstable schemas
(`schema/v1/schema.unstable.json` and its v2 counterpart), and is marked as liable
to change. An agent
must not send any of it unless the client advertises
`clientCapabilities.subagents`, but it may always run sub-agents internally and
present them through the parent session.

*Decided (2026-11-09):* the first version uses stable v1 only and presents each
delegation as one card in the parent session. Child sessions are a later
addition, gated on the client's capability (see *Child sessions, later*).

#### Reporting progress

- **A reporting path.** `run_subagent` reports only to the recorder. The turn's
  `on_event` closure is an `FnMut` on the loop's thread, is never passed to a
  sub-agent, and could not be shared with fan-out threads. *Proposed:* a
  `Send + Sync` observer on the harness (started, tool call, tool result, notice,
  finished, each carrying a per-spawn run id). `HarnessBackend` installs it next
  to `call_prompt` and reaches the editor through the same `SinkSlot`. The
  terminal interface can use the same hook for section 5. The slot holds a per-turn
  struct (the sink, the display level, the id map, and each run's log), since
  `tool_view` is a `Session` field snapshotted per turn and the hook is installed
  once, at `open()`.
- **The `delegate` call is the sub-agent's card.** Today its title is the raw
  `delegate({...})` JSON, and it stays `pending` for the whole run, because
  nothing sends `in_progress` (the spec's `pending` means not started because the
  input is streaming or awaiting approval). *Proposed:* `title()` special-cases `delegate` as
  `<name>: <first line of the task>`, a new `Update` sends
  `tool_call_update` with `in_progress` when the sub-agent starts, and later
  updates carry one line per tool call. A `content` update replaces the whole
  collection, so the backend keeps each run's log and resends it, capped at the
  last 20 lines with a count of what was dropped. The final `ToolResult` replaces
  the log with the answer, as it does now. The first `in_progress` update carries
  `content` (empty or the first line), so the approval text a project specialty's
  request put on this card does not stay there. Main-loop calls are knowingly left as
  they are, without `in_progress`. Nested runs fold into the top-level card, and
  `/agent`, which has no tool call, announces a synthetic card of its own, subject to
  `/toolview` like any other.
- **Every card ends.** Each announced card gets a final `completed` or `failed`, sent
  before the prompt is answered. On cancel it is `failed`, since `cancelled` is not a
  v1 tool status. A halted run is reported as a failure through the observer.
- **A sub-agent's calls.** *Decided:* lines in the card, not cards of their own:
  v1 has no parent field, so three sub-agents' calls in one flat list could not
  be told apart. The exception is a call the gate asks about (see *Approvals*).
- **Tool call ids must be made unique here.** The spec wants an id unique within
  the session, but provider ids are not: Ollama's are `ollama-0`, `ollama-1`,
  restarting with every response, and the OpenAI adapter falls back to `call_<n>`
  when a server sends none. This is already wrong for the main loop on those
  providers, and sub-agents make collisions certain. *Proposed:* the ACP layer
  assigns its own ids to what it reports, keeping the provider's id for the model's
  history. They come from one counter on `Session`, drawn by `replay` and live calls
  alike, so a loaded session's new calls cannot collide with replayed ids, and `/agent`
  cards use it too. An id is assigned whatever the display level, so the map is filled
  at `hidden`, where updates to a card never announced are withheld. A sub-agent's are
  `<delegate id>/<n>`: `run_subagent_tool` numbers the call at its top, with the run
  id on the forked context, and passes the gate a clone carrying the namespaced id,
  since two `reviewer` siblings on Ollama would otherwise both present `ollama-0`.
- **Display levels.** `Presenter` holds one `thought` for one stream, so sibling
  sub-agents cannot share it. Sub-agent reasoning is not sent at all in the first
  version. `/toolview` applies to the card: `full` shows the log, `summary` a
  running count, and `hidden` nothing but failures and approvals, as for the main
  loop.
- **Notices belong in the card.** `TurnEvent::Notice` becomes
  `agent_message_chunk`, so a model-fallback notice (section 2, *Model*) sent that
  way would land in the middle of the reply. From slice 7 it is a line in the card.
- **Redaction.** The card is a new path to the user. Arguments go through
  `redacted_input` and the vault. Every log line, results included, goes through the
  vault's `redact` and `show_controls`: `summarize` does neither, and a nested
  `delegate` result bypasses `dispatch`. Names and descriptions from project files go
  through `show_controls`, as `title()` does.
- **Replay shows no log.** Sub-agent histories are not saved, so `session/load`
  shows the `delegate` call and its answer without the log; only the ids change, as
  above. A known limit: `replay` hard-codes `ok: true`, so a failed call replays as
  `completed`, a failed `delegate` included.

#### Approvals

- **Saying who is asking.** `CallApprovalPrompt` gets only the call and the
  preview, so the editor cannot be told which specialty asks. *Proposed:* a label
  on the forked `ToolContext` (set at spawn, beside the allowlist of section 2),
  which `gate.decide` copies onto `ApprovalPreview`. `PermissionRequest`'s title
  becomes `[reviewer] run_command(...)`, and `approval_text` names the specialty.
- **Announce before asking.** `PermissionRequest.tool_call_id` is documented as
  already reported, but a sub-agent's call never is. The hook sends the call's
  `tool_call` first, under its namespaced id, so a card exists for what the user is
  deciding on. The hook returns before the tool runs, so the result comes from the
  observer, declines included.
- **Project specialties.** `delegate` is `side_effecting = false`, so it never
  reaches the part of the gate that asks, and approval needs its own check, the
  one in section 4. For a delegation from the main loop it runs on the loop's
  thread before any fan-out; from a sub-agent it goes through the prompt queue once
  slice 8 adds it. It
  asks in `auto` mode too. An editor session counts as somebody to ask, since it
  has a prompt even without a terminal. *Proposed:* it goes through a separate
  `approve_specialty` hook, since `call_prompt` returns a bool with fixed options.
  - The request names the `delegate` call, which the editor has already been
    shown. A sub-agent's own `delegate` call has not been, so from slice 7, when
    namespaced ids exist, it is announced first like any call the gate asks about;
    until then it is asked about unannounced, as a sub-agent's calls are today.
    `/agent` has no tool call, and `toolCall` is
    required, so it announces a card of its own first.
  - The text is the whole file through `show_controls`. The bytes that were
    hashed and shown are the bytes that run, so an edit in between cannot slip
    through.
  - *Decided:* this request offers one allowing option, kind `allow_always`,
    named for its scope ("Allow this version of reviewer"), since the agent
    owns the remembered answer (by hash), and in the spec `kind` is only a UI hint and
    the option's name states the scope. *Proposed:* the request has exactly two
    options, this one and the existing `reject_once`.
    Today every request offers `allow-once` and `permission_from` accepts only
    that, so `PermissionRequest` gains its options and `permission_from` checks
    the option the request offered. Anything else still declines.
  - An editor configured to approve automatically will answer this too. That is
    the user's setting, and the docs should say so.
- **The prompt queue (section 6) is narrower here.** The connection can already
  carry several requests at once (`pending` is keyed by id), so the queue is for
  the user's sake: one question at a time. It carries permission requests only.
  `raise_limit` has no hook in an editor session, `request_secret` is not offered
  there, and `fs/*` calls are not prompts and must not wait behind one. The check
  after acquiring the queue (section 6) applies here too.

#### Cancellation

- **Stop does not stop a sub-agent today.** `run_subagent` never checks
  `CancelToken`, and `delegate` is awaited whole. After `session/cancel` the
  sub-agent keeps calling the model and running tools (in `auto` mode, writing
  files) until it finishes or spends its budget, and the prompt is not answered
  until then. The safe points section 6 relies on do not exist yet.
  *Proposed, as its own slice before the rest:* the main loop's checks in
  `run_subagent` (before each model call, a `select!` on the model call, between
  tool calls), answering unrun calls as not executed. This also fixes Escape in
  the terminal. It differs from Python, whose sub-agent has no cancel checks
  (`agents/subagent.py`), so record it in the plan.
- **A disconnect does not stop a sub-agent either.** On EOF or a read error the pump
  sets `closed` and drops the replies but does not cancel, so sub-agents keep calling
  models and running tools that need no approval, up to their budgets. *Proposed, in
  slice 0b:* EOF or a read error sets `cancel.requested` and fires the handle.
- **No new question after a cancel.** `during_turn` drops the replies pending at
  that moment, but the pump still forwards a later `session/request_permission`.
  The pump must drop those once `cancel.requested` is set (dropping the reply
  declines). Only permission requests: letting a running tool finish is agent86's
  deliberate choice under the spec's SHOULD to stop tool calls as soon as possible, so
  `fs/*` calls it sends after the cancel still go through. Those in flight at the
  cancel are dropped with the other pending replies.
- **A cost-cap trip needs its own token.** Section 6 cancels siblings when the
  cost cap trips. If that used the turn's `CancelToken`, `HarnessBackend::prompt`
  would report `Stop::Cancelled`, which the spec reserves for the client
  cancelling. Each sub-agent run gets a token of its own, which it checks along with
  the turn's, and a trip cancels every sibling's; the turn itself then ends through
  the breaker's existing error. The same per-run token is what cancelling one child
  needs later.

#### `/agent` and `/agents`

- **`/agent` cannot be a `run_command` entry.** `run_command` runs on the
  connection's thread with no sink set, so nothing pumps while it runs: every
  approval would decline, the file tools would silently use the disk, and
  `session/cancel` would never be read. It has to run through the worker path,
  like a prompt. *Proposed:* a second list of commands that are advertised with
  `COMMANDS` but that `run_command` does not claim, so `/agent` reaches
  `HarnessBackend::prompt`, which runs it with the sink slot set as a turn does.
- **One entry point with the REPL** (section 3). The harness method behind
  `/agent` must do what `run_turn_events` does for a turn: clear the cancel token
  on entry, redact the user's text and the answer through the vault, apply the ingress
  and egress guardrails (the answer goes straight to the user, not through a parent),
  route the task when the specialty has no `model` (section 3), add usage to
  `state.usage` so `/cost` is right, emit `turn_start` and `turn_end`, set `last_turn`
  and the phase, record the episode, and be persisted.
  The exchange is saved as ordinary user and
  assistant messages, so `replay` and `--resume` work unchanged.
- **`/agents [name]`** is a plain `run_command` entry, reading the session's own
  list. An unknown name for either command is answered without a model call.
  `commands()` is not per-session, so specialty names are not offered as
  completions; the hint is `[--with-context] <name> <task>`.

#### Workspace and connection

- **Discovery uses the session's directory.** It belongs in `setup::assemble_in`,
  so `<cwd>/.agent86/agents/` is the editor's project, not where `agent86 acp` was
  started.
- **Worktrees (section 7).** Besides clearing `ToolContext::editor`:
  - A file the agent wrote through the editor may exist only in a buffer, so the
    dirty-workspace notice has to cover unsaved editor changes as well.
  - The startup sweep is unsafe with several editor windows, each its own
    `agent86 acp` process: one starting up would delete another's live worktree.
    The sweep may remove only worktrees whose owning process is gone, so the name
    or a marker file has to record the owner.
- **One turn at a time still holds.** While a turn runs, `session/new` and
  `session/set_config_option` are refused. Long delegations make this more
  visible; it is a documented limit, not a change in this work.

#### Child sessions, later

**Where things stand (checked 2026-11-09).**

- **The spec.** The [RFD](https://agentclientprotocol.com/rfds/subagents) is
  active, sits behind `unstable_subagents`, and asks for validation against Claude
  Code and Codex before it is stabilized. It is still moving: the latest revision
  ([#2775](https://github.com/agentclientprotocol/agent-client-protocol/pull/2775),
  on how a child's first task is reported) merged on the day of this check. There
  is no stabilization date.
- **Agents.** `claude-agent-acp` sends the RFD's form to a v1 client that advertises
  `subagents` ([#1320](https://github.com/agentclientprotocol/claude-agent-acp/pull/1320)),
  and `codex-acp` has added live RFD sessions
  ([#682](https://github.com/agentclientprotocol/codex-acp/pull/682)).
- **Zed.** The client side merged in October (child routing, inline cards, the
  cancel rules), but on `main` a v1 connection advertises `subagents` only behind
  the `acp-beta` feature flag (`client_capabilities_for_agent` in
  `crates/agent_servers/src/acp/v1.rs`, pinned by a test). Without the flag, an
  agent may not send any of this.
- **JetBrains Air.** According to `claude-agent-acp`'s #1320, Air implements an
  earlier draft (`subagent_spawned`, `subagent_state_update`), identified by
  `_meta.jetbrains.air`.
- **Other clients.** A search found no evidence either way, so they are unconfirmed.

**Decided (2026-11-09).** Leave child sessions out of the first
version, and build them as their own slice, after slices 7 and 8, once **Zed
offers `subagents` without `acp-beta` in a stable release**. That is when users
would see a difference. Until then the audience is people with a beta flag on,
and the cost is routing every message by session while tracking a schema that
still changes.

- If the RFD is still unstable at that point, build against a pinned commit of
  `schema/v1/schema.unstable.json`, and repeat the transcript check against it.
  The client's capability remains the only gate. Read the RFD again first.
- Do not implement Air's earlier draft. It is a second wire format for one client,
  and the RFD replaces it.
- The first version already builds most of what this needs: the observer, per-run
  ids and tokens, and unique tool call ids. The one addition is for the observer
  to report when a sub-agent starts and stops waiting on an approval, and why it
  finished, so its events map onto the RFD's states.

**What changes then:**

- `ClientCapabilities::from_params` reads the capability. There is no agent-side
  capability to advertise.
- Everything belonging to the child's work carries its session id: updates,
  permission requests, and `fs/*` calls. The specialty approval before the spawn and
  the `delegate` call stay on the parent, since the RFD keeps a parent's own request
  to create or delegate to a child as a parent interaction. `TurnMsg::Update` and
  `ChannelSink` assume one session today.
- `subagent_update` goes out before any traffic naming the child, with `title`,
  `description`, `state`, and `capabilities: { "cancel": {} }`. A worktree child
  also reports `cwd` and `additionalDirectories: []`. `state` is an object: `running`,
  `requires_action` while a permission request is open (`running` and `idle` are
  required, this one recommended), and at the end
  `{ "state": "idle", "stopReason": "end_turn" }`. A failure is `stopReason: "error"`
  with an optional `error`, the only place a v1 client learns of it.
- The task is a `session_message` in the child's transcript with `senderSessionId`
  set to the parent, not a `user_message_chunk`. The parent also sends its own
  `session_message` with `recipientSessionId` set to the child, and `messageId`,
  `senderMessageId`, and `recipientMessageId` link the two views. Zed anchors its
  inline card on the parent's outgoing record of that message
  ([#66040](https://github.com/zed-industries/zed/pull/66040)), so without one
  the child would get no card (inferred from the PR's text, not tested).
- The `delegate` call is still reported as a tool call, as the RFD allows. Check in
  Zed that it and the child's card do not read as duplicates.
- `session/cancel` naming a child stops only that child, through its per-run
  token, and the child reports `idle` with `stopReason: "cancelled"`, promptly, since
  cancelling the parent does not resolve a descendant's requests until it does.
  Cancelling the parent cascades to the children it waits on. Every other mutation on
  a child is refused (prompting, config changes, close, fork, load, and resume), and
  children are left out of `session/list`.
- Replay can be parent-only, since the RFD treats child history as best effort.
- A client without the capability keeps getting the card described above.

**ACP v2 is a separate decision.** agent86 negotiates v1 only
(`PROTOCOL_VERSION`). The RFD plans child sessions as part of v2's baseline, but
the [migration guide](https://agentclientprotocol.com/protocol/v2/migration.md)
still lists them as draft only, and `subagent_update` is in
`schema/v2/schema.unstable.json`, not `schema/v2/schema.json`. v2 is itself a draft
with no date, and it removes `fs/*` (which `AcpFiles` is built on), `terminal/*`,
`session/load`, and the modes API. Moving to it is larger than this work and would
not by itself deliver child sessions, so the two should not be tied together.

#### Tests and docs

- `tests/acp.rs` has no delegation test today. It needs: a sub-agent's card and
  its `in_progress` update, unique ids with Ollama-style repeats, an approval from
  a sub-agent, cancel during a delegation (no later permission request, answered
  `cancelled`), and `/agent` asking for approval mid-run. The scripted `Backend` in
  `src/acp/mod.rs`'s tests covers the protocol side.
- Repeat the schema check on a transcript that includes all of the above, and try
  it by hand in Zed, since how an editor draws a long-running card and its
  approvals is not something the schema shows.

## Reference

The behaviour here is as its section says, including what is marked *Proposed* there.

**Frontmatter keys** (`<name>.md`):

| Key | Type | Default | Meaning | Invalid value | Section |
| --- | --- | --- | --- | --- | --- |
| `name` | string | required | Must equal the file's stem | Skip the file | 1 |
| `description` | string | required | Listed to the model once permitted | Skip the file | 2, 4 |
| `tools` | list | Parent's tools, minus `delegate` | Allowlist; present but empty means none | Skip the file | 2 |
| `model` | `provider:model` | Turn's model | That specialty's model | Not invalid: falls back (sub-agent) or fails (`run --agent`) | 2, 3 |
| `max_steps` | integer, at least 1 | `[agents] max_steps` (the turn's budget under `run --agent`) | Its step budget | Skip the file | 2 |
| `approval` | `auto`, `ask`, `deny` | Session's | Applies only if stricter | Skip the file | 2 |
| `confirm` | list | None | Tools always asked about | Skip the file | 4 |
| `skills` | list | None | Skills to preload | A malformed list skips the file; an unknown name is ignored with a notice | 8 |
| `isolation` | `worktree` | Shared workspace | Runs in a worktree | Skip the file | 7 |

A skill's `SKILL.md` gains one key, `agent`, naming a specialty to hand off to (section 8).

**`delegate` arguments:**

| Argument | Type | Required | Meaning | Section |
| --- | --- | --- | --- | --- |
| `role` | string | No, default `assistant` | Generic persona; ignored if `agent` is given | 2 |
| `task` | string | Yes | The self-contained task | 2 |
| `agent` | string | No | The specialty to run | 2 |
| `isolate` | boolean | No, needs `agent` | Run in a worktree | 7 |
| `skill` | string | No, needs `agent` | Preload this skill for this run | 8 |
| `files` | list of strings | No | Workspace paths to attach | 9 |
| `summary` | string | No | Context the parent wrote | 9 |

**Commands and flags:**

| Form | Surfaces | Section |
| --- | --- | --- |
| `/agent [--with-context] <name> <task>` | Interface, plain REPL, editors | 3, 10 |
| `/agents [name]` | Interface, plain REPL, editors | 3, 10 |
| `#name` in a prompt | Interface and plain REPL | 3 |
| `agent86 run --agent <name> <goal>` | `run` | 3 |
| `--trust-project`, `AGENT86_TRUST_PROJECT` | Approves project specialties, but not in an editor | 4 |

**Configuration and files:**

| Item | Value | Section |
| --- | --- | --- |
| `[agents] max_parallel` | Integer, default 3; below 1 counts as 1; ceiling 8; trust-free | 6 |
| `~/.agent86/agents/<name>.md` | User specialties | 1 |
| `<workspace>/.agent86/agents/<name>.md` | Project specialties | 1 |
| `~/.agent86/approved-agents.json` | Project approvals, by resolved path plus SHA-256 | 4 |

## Settled questions

Settled (2026-10-31):

- **Worktrees see committed work only** (section 7).
- **`[agents] max_parallel = 3`**, with no separate per-provider cap for now.
- **Built-in tool lists** (section 4).
- **Python compatibility:** specialty sessions and traces stay readable by
  Python (`role` is the specialty name, no new event kinds). Approvals-by-hash are
  stored outside `state_json` and the session database, in a file under `~/.agent86/`
  (see *Reference*). Worktrees and their owner markers live in the temporary directory
  (section 7).
- **Frontmatter names** (`name`, `description`, `tools`, `model`) stay close to
  Claude Code's, so a `.claude/agents` reader is cheap to add later.

Settled (2026-11-09):

- **How an editor shows a sub-agent:** one card per delegation with its calls as
  lines, and a card of its own only for a call the gate asks about (section 10).
- **A project specialty's approval in an editor:** one `allow_always` option, with
  no separate allow-once (section 10).
- **When to add ACP child sessions:** as their own slice after slices 7 and 8,
  once Zed offers `subagents` without `acp-beta` in a stable release; not tied to
  ACP v2 (section 10).
- **What the model sees of an unapproved project specialty:** its name only,
  marked as needing approval (section 4).
- **How a skill hands off to a specialty:** `use_skill` asks the model to call
  `delegate`, rather than starting the sub-agent itself (section 8).
- **`run --agent` with `--model`:** the explicit flag wins over the specialty's
  `model`, and whichever applies bypasses triage routing (section 3).
- **`/agent` with no `model:`** routes on its task as a turn does (section 3).
- **A specialty with no `tools` line** inherits everything except `delegate`
  (section 2).
- **The `confirm:` frontmatter key** names always-asked tools (section 4).
- **Where specialty delegations run in a batch:** after reads, in call order; a
  delegation waits for any write listed before it, and only adjacent delegations
  run together (section 6). Any batch containing a specialty delegation takes the
  reads-first path, so this holds whatever the number of reads.
- **`--trust-project` in an editor:** it approves no project specialty there; the
  editor still asks (section 4).
- **A refused or declined project specialty** does not fall back to a built-in or
  user file of the same name (section 4).
- **`run --agent` with `isolation: worktree`** is refused (section 3).
- **`[agents] max_parallel`** has a ceiling of 8, and a value below 1 counts as 1
  (section 6).

No decision blocks slice 0. What is still marked *proposed* is the recommended
answer, to confirm when its slice is built. Some of it is behaviour rather than
implementation detail, such as how nesting narrows (section 2), the step-budget
order (section 2), and when a skill hand-off falls back (section 8). Still to write
or set: the built-in specialties' prompts, and the size caps for `files` (start from
`[tools] mention_max_bytes`) and for the worktree diff.

## Rough slices (provisional)

0. Fix sub-agent routing: send the request to the routed provider, on its own
   and first, since plain `delegate` changes too. Then, also on its own and for
   the same reason, slice **0b**: make sub-agents cancellable (the main loop's safe
   points in `run_subagent`, returning partial usage and the halted-style result of
   section 6), stop the ACP pump forwarding permission requests after a cancel, and
   make EOF or a read error cancel the turn.
1. Parse and discover definitions (`src/agents_def.rs`, pure, tested with
   fixtures; no behavior change), with ASCII names and fail-closed parsing.
2. `delegate`'s `agent` argument, persona replacement, filtered system prompt (with
   its parts stored and the budget counting it), system-prompt listing. Project files
   stay ignored until slice 5, so no unapproved repo text can reach the model in
   between.
3. Tool allowlist (its own context field, checked in `allowed_tool` and in the gate),
   per-specialty approval mode, always-asked tools, per-specialty step budget with
   `raise_limit`'s scope, and narrowing carried through nested delegation (forking the
   caller's context).
4. Per-specialty model, with its own context window and output cap, the run-only raise
   of that cap, the fallback's `route` event, and the per-turn notice list that tells
   the user.
5. Project-specialty trust, which turns project files on: approval by content hash
   in the one check every start goes through, the whole file shown, name-only listing
   until approved, `--trust-project` carried separately (and not honoured in editors),
   and the editor's request through `approve_specialty` with its `allow_always` option.
6. Context passing (`delegate`'s `files` on the caller's authority, and `summary`),
   with redaction and the observation guardrail applied to what is attached.
7. User entry points (`/agents`, `/agent` on the turn worker in the terminal and
   in editors, with routing and `--with-context` through the extracted summarizer,
   `#name` as a hint with `#` menu completion, `run --agent` with its routing bypass,
   early resolution, main-loop spec filtering, and its errors), the sub-agent
   observer, and interface display. For ACP: the `delegate` card with `in_progress`,
   its log, and its final update, `/agent`'s card, the layer's own tool call ids shared
   with `replay`, attributed approvals, announcing a nested `delegate`, and the
   fallback notice as a card line.
8. Parallel fan-out of adjacent delegations on scoped threads, each with its own
   HTTP client, with `[agents] max_parallel`, the reads-first batch rule, the
   `PromptQueue`, the per-turn spend, the events' `run` field, and a cancellation token
   per run (the riskiest slice; needs tests for ordering against writes, both of
   `execute_batch`'s paths, `parallel_tools = false`, cancellation, a reused
   keep-alive connection, approval prompts interleaving in the terminal and over ACP,
   and limit and secret prompts interleaving in the terminal).
9. Worktree isolation from `HEAD`, including the dirty-workspace notice, `SandboxPolicy`'s
   re-root, hardened host `git`, the redacted diff with untracked files, nested
   delegations staying in the worktree, MCP tools left out unless named, clearing the
   editor hook, a sweep that spares other processes' worktrees, and the system prompt's
   advice to isolate parallel writers.
10. Skills both ways (preloading, `delegate`'s `skill` argument, and `use_skill`'s
    hand-off note with its fallback, with the context fields it needs).
11. Built-in specialties (section 4's tool lists), embedded and tested for the prompt
    text and tool lists.
12. Final docs pass, the *When this lands* list, and README status. Each slice records
    its own differences from Python in the plan as it lands.

Slices 2 to 4 are not to be released separately: a specialty's `tools:` and `model:`
take effect only by slice 4.

Later, and not numbered: ACP child sessions, once Zed offers `subagents` without
`acp-beta` in a stable release (section 10).

Each slice lands with tests that pin the contract; none needs credentials or
network (the mock server covers delegation already).

## When this lands

These still describe today's behaviour and change with the slice that ends it:

- `docs/ACP.md`: the command count (and "one of the four", already stale beside
  "Eight slash commands"), "there is no always allow", "Remembered permission
  choices" under what is not built (this one option is the exception), and the
  project-file trust paragraph, which recommends `--trust-project`: that flag does not
  approve project specialties in an editor.
- `docs/adr/0002-acp-server.md`: "the one allowing option" and the list of what is
  advertised.
- `docs/PROJECT_PLAN.md`: the "not implemented" entry for remembered permissions, and
  the delegation entries in section 3, as each difference from Python lands.
- `README.md`: the `--trust-project` line, the line saying skills and sub-agents get
  the same tools and approvals, and add a roadmap line, which does not exist yet.
- `docs/INTERFACE.md`: the `#` menu and the specialty label on approvals.
- `src/main.rs`: `--trust-project`'s help text.
- `CLAUDE.md`: the entries for `src/acp/`, `src/trust.rs`, and `src/subagent.rs`
  (`ToolContext::fork`). Fan-out also ends two statements that are true today: that
  approvals are always settled on the loop's thread (`src/approval.rs`) and that
  `delegate` always runs in the batch's sequential part (`src/subagent.rs`).
