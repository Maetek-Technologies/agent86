# Scheduled jobs: the agent schedules work for itself

**Status:** design (not yet implemented)
**Last updated:** 2026-10-31

This document is the plan for letting the model create durable, recurring (or
one-shot) jobs — "every morning at 9, do X in this workspace" — and having
agent86 itself fire them and run a real agent turn to do the work, even when no
interactive session is open. The agent schedules work *for itself*; agent86
executes it.

This is entirely **beyond Python** (Python has no scheduler), so it is a
deliberate addition to record in the project plan, with no compatibility surface
to preserve.

## Goal

Let the model create durable, recurring (or one-shot) jobs and have agent86
itself fire them and run a real agent turn to do the work, even when no
interactive session is open. The agent schedules work *for itself*; agent86
executes it.

## Key design decisions (and why)

### 1. Where jobs live — a separate file, not the memory DB

Store jobs in `~/.agent86/schedule.json` (path overridable via `[schedule]
path`), written atomically (temp + rename, the same pattern `update.rs` and
`config_writer` use). Rationale: it decouples the scheduler from the memory
subsystem (which requires building an embedder and is off when `[memory]
enabled = false`), and it doesn't touch the Python-shared SQLite schema. Job
*runs* still land in the memory DB as normal sessions/episodes; only the job
*definitions* live in the file.

(Alternative: a `jobs` table in `memory.db` — rejected for the coupling reasons
above.)

### 2. Who fires jobs — a resident daemon, primary

A new `agent86 schedule daemon` subcommand: a long-running process that loads
the job list, sleeps until the next fire time, and runs each due job. This is
the only mechanism that performs tasks on a schedule with no session open (the
actual "every morning at 9" case). Two secondary mechanisms, deferred:

- *In-process check* in the interactive loop's 50 ms `pump` (fires short
  reminders while you're using agent86, and lets you approve a job's side
  effects at the terminal). Follow-up.
- *OS trigger* (the agent installs a cron/Task Scheduler entry via
  `run_command`). Documented as an option, never automatic — it's
  platform-specific and fragile.

### 3. Schedule representation — `at` / `every` / `daily_at`, no cron parser

- `at`: one-shot timestamp (RFC 3339).
- `every`: interval in seconds.
- `daily_at`: `HH:MM` in a timezone (default `chrono::Local`, optional explicit
  offset).

This covers the common cases without adding a cron dependency (the codebase is
deliberately lean on deps). Cron is a possible later extension.

### 4. Execution model — a fresh, cost-capped, unattended turn per fire

When a job fires, the daemon assembles a harness for the job's `workspace` +
`model` + `approval` and runs `Harness::run_turn` with the job's `task` as the
goal, in a **fresh session** (bounded context, no drift across runs; continuity
comes from the memory DB's episode recall plus a short note of the last run's
outcome). Approval defaults to `auto` (unattended — `ask` declines with no
prompt, which is exactly the safe behavior we want). Every run is bounded by the
existing `CircuitBreaker` (`max_steps`, `max_cost_usd`, `max_wall_clock_s`,
`max_consecutive_errors`), so a runaway job can't spend unbounded. One job run
at a time (a lock file, `~/.agent86/schedule.lock`, also prevents two daemons
double-firing).

### 5. Missed-fire / catch-up semantics (explicit, configurable)

On daemon start, for each enabled job compute its next fire from the last run.
If it's in the past: a **recurring** job fires *once* (then advances its next
fire to the first occurrence strictly after now — no burst of catch-up fires);
a **one-shot** job fires once if missed by less than a grace window, else is
marked `missed` and disabled.

### 6. Security — creation is approval-gated and everything is visible

- `schedule_create` is **side-effecting**, so the user approves the job
  definition (task, schedule, workspace, model) before it's persisted — a model
  can't silently install a recurring action.
- Jobs are always inspectable and removable by the user: `agent86 schedule
  list|show|cancel` and (later) a `/schedule` panel.
- Job task text passes through the **ingress guardrail** like any goal.
- **No session secrets** in a job run (unattended → `request_secret` isn't
  offered); provider keys come from the keyring as usual. Documented limitation.
- The daemon refuses to run a job whose workspace is gone or whose model can't
  be built — it records a failure, never guesses.

## Components to build (mapped to files)

- **`src/schedule.rs`** (new): the `Job` model (id, task, schedule, workspace,
  model, approval, created_at/by, enabled, last_run_at/status/output,
  run_count); the schedule math (`next_fire(now)`, `advance_after(fire_time)`);
  the `Scheduler` store (load/save with atomic write, the lock file); and the
  daemon loop + `run_job` (assemble harness → run turn → record result →
  persist → emit trace/episode).
- **`src/tools/schedule.rs`** (new): `schedule_create`, `schedule_list`,
  `schedule_cancel` (and optionally `schedule_run`). `schedule_create` is
  side-effecting; the others are read-only. A `Scheduler` handle rides on
  `ToolContext` (exactly like `memory` does).
- **`src/setup.rs`**: refactor `assemble` to take an explicit workspace (it
  currently hardcodes `current_dir()`), so the daemon can build a harness jailed
  to the job's workspace. Keep the existing call sites passing `current_dir()`.
- **`src/tools/mod.rs`**: register the schedule tools in `default_registry`
  (gated on the feature being on), and add the `Scheduler` field to
  `ToolContext`.
- **`src/main.rs`**: the `schedule` subcommand — `list`, `show <id>`,
  `cancel <id>`, `run <id>` (run now, for testing), `daemon`.
- **`src/config.rs`**: a `[schedule]` section — `enabled`, `path`,
  `max_concurrent` (default 1), `grace_s`, `log` path, `default_model`.
- **`src/observability.rs`**: new trace events (`job_created`, `job_run` with
  status/usage/cost, `job_missed`), redacted like everything else.
- **`src/repl.rs` / `src/ui/`** (later): a `/schedule` command and, in the
  interface, a schedule panel + the in-process due-job check.
- **Tests** (see below) and **docs** (README status/roadmap,
  `PROJECT_PLAN.md`, `CLAUDE.md`, and a new **ADR 0002** — this is a significant
  design decision, matching how ADR 0001 handled the interface).

## Phased implementation (each a testable vertical slice)

1. **Job model + schedule math + store.** Pure, no I/O beyond the file.
   Unit-test `next_fire`/`advance_after` for all three schedule kinds across
   timezone boundaries, DST, and the catch-up rules; test store load/save
   round-trip, atomicity, and the lock.
2. **Execution + `schedule run <id>`.** Wire `run_job` (refactored `assemble` +
   `run_turn` + persist + trace). CLI test: create a job by hand, `schedule run
   <id>` against the mock provider in an isolated HOME, assert the turn ran, the
   session persisted, and the result was recorded.
3. **The daemon.** The loop, the lock file, catch-up on start, one-at-a-time
   execution, clean shutdown on Ctrl+C. Integration test: a job with a short
   `every` interval, mock provider, isolated HOME — assert it fires, advances,
   and doesn't double-fire.
4. **The tools.** `schedule_create` (approval-gated, writes to the store),
   `schedule_list`, `schedule_cancel`. Tool tests: creation is refused under
   `deny`, approved under `auto`/`ask`+prompt, and lands in the store; the model
   only ever sees the job's fields, never a secret.
5. **Config, trace events, notifications.** `[schedule]` section; `job_*` trace
   events; the daemon logs runs to a file and the interface shows a note when a
   job fires in-process.
6. **Docs + ADR + plan.** README, `PROJECT_PLAN.md` (recorded as a deliberate
   addition), `CLAUDE.md` architecture note, ADR 0002.

## Testing strategy (matching the repo's conventions)

- No network/credentials in default tests: mock provider + recorded fixtures,
  isolated `HOME`/`USERPROFILE` and working directory (the `tests/cli_run.rs`
  pattern).
- Schedule math is pure → exhaustive unit tests (the highest-value, cheapest
  tests).
- Store: round-trip, atomic write (simulate a crash mid-write), lock
  contention.
- Daemon: short-interval job against the mock server; assert fire, advance,
  single-fire catch-up, and clean shutdown.
- Tools: approval gating and store effects; `schedule_create` under
  `deny`/`auto`/`ask`.
- Failure paths: bad model, missing workspace, cost-cap trip — each records a
  clean failure, never panics.

## Risks / open questions

- **Daemon lifecycle on a laptop:** a resident process won't fire while the
  machine is asleep. The catch-up rule handles "I was asleep at 9am," but for
  hard deadlines the OS-trigger option is the robust answer. Worth stating
  plainly in the docs.
- **Harness build cost per fire:** building a harness (provider + MCP servers)
  per job run is correct but wasteful for frequent jobs. v1 builds-and-drops
  per run; a future optimization pools harnesses keyed by `(workspace, model)`.
- **Unattended + `auto` approval is the real risk surface.** Mitigations are
  the approval-gated creation, visibility, cost caps, and the ingress
  guardrail — but a buggy/malicious model can still schedule something the user
  approved once. The ADR should make the threat model explicit.
- **Secrets:** jobs can't use session secrets (no one to prompt). Only
  keyring/env provider keys are available. Documented limitation.
- **Naming:** this plan uses `agent86 schedule …` with `daemon` as a
  subcommand; it could be split to a top-level `agent86 daemon` if it should
  stand out.
