# Scheduled jobs: the agent schedules work for itself

**Status:** design (not yet implemented); targeted for 1.0.25, together with
parking a job at an approval (see the release roadmap in
`docs/PROJECT_PLAN.md`)
**Last updated:** 2026-11-15 (decision 2 and the new decision 7: how the OS
fires jobs). `docs/PROJECT_PLAN.md` also records the maintainer's decisions of
2026-10-31 (one release, desktop notifications, per-job approval, results);
where this document and the plan differ, the plan wins.

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

### 2. Who fires jobs — the OS's own scheduler, calling `agent86 schedule tick`

Revised 2026-11-15. The first version of this decision made a resident daemon
primary and called OS triggers fragile. The fragility it meant was the *model*
writing cron or Task Scheduler entries through `run_command`, which stays
ruled out. agent86 writing one well-tested entry itself is a different thing,
and the OS schedulers handle the case a resident loop cannot: a machine that was
asleep at fire time.

- **`agent86 schedule tick`** runs every job that is due, once, under the lock
  file, and exits. All firing goes through one function (`run_due(now)`), with
  the catch-up rules of decision 5 applied on every call, so a tick after a
  night asleep fires each missed recurring job once.
- **`agent86 schedule install-service`** writes one per-user OS entry that runs
  `tick` every minute and again after login and wake (decision 7). There is one
  entry for all jobs, so creating or cancelling a job never touches the OS
  scheduler: `schedule_create` only writes `schedule.json`, and the next tick
  sees it. `uninstall-service` removes the entry; `schedule status` says
  whether it is installed and whether its binary path still exists.
- **`agent86 schedule daemon`** stays, for machines with no per-user service
  manager or where the user prefers a foreground process (a container, tmux, a
  server): the same `run_due` in a loop that sleeps until the next fire time.
- *In-process check* in the interface's 50 ms `pump` (short reminders while
  agent86 is in use, with side effects approvable at the terminal) is still a
  follow-up.

A tick that finds nothing due reads one small file and exits, so a one-minute
interval costs little; jobs fire within a minute of their time, which suits
`daily_at` and `every` intervals of minutes or more. The interval is
`[schedule] tick_s` (default 60), written into the entry at install.

### 3. Schedule representation — `at` / `every` / `daily_at`, no cron parser

- `at`: one-shot timestamp (RFC 3339).
- `every`: interval in seconds.
- `daily_at`: `HH:MM` in a timezone (default the system time zone through `jiff`, optional explicit
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
- `install-service` is a command the user runs, never a tool: the model can
  create jobs (approved), but cannot make the OS run agent86.

### 7. OS integration — per-user entries, written by agent86, in Rust

Added 2026-11-15.

**Per user, never system-wide.** Provider keys live in the OS keyring, and
every keyring is tied to the user's login session: the Secret Service on Linux
needs the session D-Bus, a LaunchDaemon on macOS cannot open the login
keychain, and a Windows service running as LocalSystem cannot see the user's
Credential Manager. A system service would run jobs with no API keys (the plan
already rejected a boot-time service for this reason). No admin rights are
needed and nothing outside the user's own configuration changes.

| OS | Entry | Fires `tick` | After sleep or a missed run |
|---|---|---|---|
| Linux | systemd user units `agent86-schedule.service` (oneshot) and `.timer` in `~/.config/systemd/user/`, then `systemctl --user enable --now` the timer | `OnCalendar` every `tick_s`, plus `OnStartupSec` | `Persistent=true` runs a missed elapse at once |
| macOS | LaunchAgent plist in `~/Library/LaunchAgents/`, then `launchctl bootstrap gui/<uid>` | `StartInterval` = `tick_s`, plus `RunAtLoad` | launchd fires a missed interval on wake |
| Windows | Task Scheduler task from an XML definition (`schtasks /Create /XML`), running as the current user, "only when the user is logged on" | a logon trigger with a repetition of `tick_s` | `StartWhenAvailable` runs a missed start as soon as possible |

The Windows task never uses "run whether the user is logged on or not", which
would make Windows store the user's password and run outside the session that
can read Credential Manager. Waking the machine for a job (Task Scheduler's
`WakeToRun`, `pmset` schedules, systemd's `WakeSystem=`) is left out: it
changes power settings, and the catch-up rule covers a sleeping laptop.

**The binary path must outlive updates.** The entry names a binary, and
updates move binaries (on Windows, `/update` moves the running one aside). The
path comes from `update::Install::detect`: the npm launcher on PATH for an npm
install, `$CARGO_HOME/bin/agent86` for cargo, and the running executable for an
archive install. `schedule status`, and each `tick` (in the run log), say when
the entry's path no longer exists.

**A service starts with almost no environment.** launchd and Task Scheduler
do not read shell startup files, so variables set there (an `api_key_env` key,
`PATH` additions for MCP servers launched with `npx` or `uvx`) are missing.
Provider keys come from the keyring, which is the supported path; the entry
never contains a key, and `install-service` warns (without refusing) about
each configured provider whose key is only in the environment. `PATH` is not a
secret: `install-service` records the current `PATH` in the entry and shows it.

**Headless Linux.** A systemd user timer runs only while the user is logged in
unless `loginctl enable-linger` is on, and with linger but no login the Secret
Service is usually not running or is locked. A job that cannot read its key
records a clear failure ("keyring unavailable"), never waits for an unlock
prompt. On such machines `schedule daemon` with keys in the environment the
user sets up, or `enable-linger` with an unlocked keyring, are the documented
options; agent86 never writes a key into a unit file.

**Logs.** `tick` appends to the run log itself (`[schedule] log`), so the log is
the same on every OS whatever the service manager does with stdout.

**Hand-written, no dependency.** The three definitions are small, pure
functions of the binary path, `tick_s`, `PATH`, and the home directory, and
the generic service crates (`service-manager`) aim at long-running services,
not per-user timers with catch-up. The commands (`systemctl`, `launchctl`,
`schtasks`) run through a small trait, so tests record them with a fake runner
and never touch a real service manager.

**Considered and rejected: a Go helper for the OS parts** (2026-11-15). Go's
service libraries are good, but the OS work is three templates and a few
commands, and the daemon's real work (`run_job`: build a harness, run a turn,
record it) is Rust either way, so the helper could only start and stop
agent86, which the OS already does. It would add a second toolchain to CI and
the five-target release, a second advisory scanner, two binaries in every npm
platform package, and a second codebase to security-review, and it would end
the single native binary. Go's service libraries also default to system
services, which lose the keyring.

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
- **`src/service.rs`** (new): the three per-user definitions (decision 7) as
  pure functions, the binary path from `update::Install::detect`, and
  install, uninstall, and status through a command-runner trait.
- **`src/main.rs`**: the `schedule` subcommand — `list`, `show <id>`,
  `cancel <id>`, `run <id>` (run now, for testing), `tick`, `daemon`,
  `install-service`, `uninstall-service`, `status`.
- **`src/config.rs`**: a `[schedule]` section — `enabled`, `path`,
  `max_concurrent` (default 1), `grace_s`, `tick_s` (default 60), `log` path,
  `default_model`.
- **`src/observability.rs`**: new trace events (`job_created`, `job_run` with
  status/usage/cost, `job_missed`), redacted like everything else.
- **`src/repl.rs` / `src/ui/`** (later): a `/schedule` command and, in the
  interface, a schedule panel + the in-process due-job check.
- **Tests** (see below) and **docs** (README status/roadmap,
  `PROJECT_PLAN.md`, `CLAUDE.md`, and a new ADR (the next number; 0002 is the ACP server) — this is a significant
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
3. **`tick` and the daemon.** `run_due(now)` with the lock file and the
   catch-up rules, `schedule tick` (one pass, then exit), and the daemon loop
   over the same function with a clean shutdown on Ctrl+C. Integration tests:
   two overlapping ticks fire a due job once; a tick after a gap catches up
   once; a job with a short `every` interval under the daemon fires, advances,
   and doesn't double-fire.
4. **The tools.** `schedule_create` (approval-gated, writes to the store),
   `schedule_list`, `schedule_cancel`. Tool tests: creation is refused under
   `deny`, approved under `auto`/`ask`+prompt, and lands in the store; the model
   only ever sees the job's fields, never a secret.
5. **Config, trace events, notifications.** `[schedule]` section; `job_*` trace
   events; the daemon logs runs to a file and the interface shows a note when a
   job fires in-process.
6. **`install-service`, `uninstall-service`, `status`** (`src/service.rs`).
   Tests: each OS's definition against a golden file on every platform (the
   binary path, `tick_s`, `PATH`, no key), the commands recorded by the fake
   runner, the binary path per install kind, and a missing path reported by
   `status`. An opt-in live test per OS, like `tests/os_keyring.rs`, installs,
   sees one tick, and uninstalls; it stays out of CI.
7. **Docs + ADR + plan.** README, `PROJECT_PLAN.md` (recorded as a deliberate
   addition), `CLAUDE.md` architecture note, an ADR (the next number).

The plan adds desktop notifications as their own phase; see
`docs/PROJECT_PLAN.md`.

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

- **A sleeping laptop:** nothing fires while the machine is asleep. The OS
  entry (decision 7) runs a tick soon after wake, and the catch-up rule fires
  each missed job once, so "I was asleep at 9am" runs the job at wake, not at
  9. A hard deadline on a machine that sleeps needs the machine awake; agent86
  does not change power settings. The docs should say this plainly.
- **Service managers differ in the details** (launchd's handling of missed
  intervals, systemd user sessions without linger, Task Scheduler's battery
  conditions, which the XML must turn off). These are the likeliest parts to
  need fixes after the first live run on each OS; the release notes should say
  which platforms were run live.
- **Harness build cost per fire:** building a harness (provider + MCP servers)
  per job run is correct but wasteful for frequent jobs. v1 builds-and-drops
  per run; a future optimization pools harnesses keyed by `(workspace, model)`.
- **Unattended + `auto` approval is the real risk surface.** Mitigations are
  the approval-gated creation, visibility, cost caps, and the ingress
  guardrail — but a buggy/malicious model can still schedule something the user
  approved once. The ADR should make the threat model explicit.
- **A job that needs an approval:** in this design an unattended run declines
  every question, so the job fails at that point. Parking it instead (save,
  stop, resume from any surface with the question asked again) is proposed in
  the plan ("Supervision and long-running work", item 4) and needs checkpointing
  tiers 1 and 2 first (`docs/checkpointing.md`), so it comes after this
  release.
- **Secrets:** jobs can't use session secrets (no one to prompt). Only
  keyring/env provider keys are available. Documented limitation.
- **Naming:** decided (see the plan): `agent86 schedule …`, with `daemon`,
  `tick`, and the service commands as subcommands.
