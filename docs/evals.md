# The eval set

Slice 0 of the local-model work in [PROJECT_PLAN.md](PROJECT_PLAN.md): a set of
repository tasks with hidden checks, so that every later change to the harness
(the verification gate, tool output sized to the window, the stall nudge, and the
rest) is measured instead of claimed. It is a development tool, never built by
the release workflow and never run by CI against a model.

## Running it

```text
cargo build --release
cargo run -p agent86-eval -- list
cargo run -p agent86-eval -- validate
cargo run -p agent86-eval -- run --model vllm:cyankiwi/Qwen3.6-35B-A3B-AWQ-4bit
cargo run -p agent86-eval -- run --model vllm:cyankiwi/Qwen3.6-35B-A3B-AWQ-4bit \
    --model openrouter:anthropic/claude-sonnet-4.6 --repeat 3 --window 32768 --jobs 2
cargo run -p agent86-eval -- run --model lmstudio:google/gemma-4-12b-qat \
    --variant evals/variants/drop.toml --category long-context
```

- `--model` (repeatable) is any `provider:model` ref agent86 can run. Providers
  come from the `[providers]` and `[pricing]` tables of your own
  `~/.agent86/config.toml` (so a vLLM `base_url` works), and keys from the
  environment or the OS keyring, as the CLI resolves them. Nothing else of your
  config is used: no MCP servers, skills, or routing. `--no-user-providers` leaves
  even those out.
- `--task` and `--category` (repeatable) choose tasks; all of them by default.
- `--repeat N` runs each task N times per model and variant. Models are not
  deterministic even at temperature 0, so compare rates over several repeats.
- `--variant file.toml` (repeatable) is a config overlay to compare with the
  baseline, named after the file's stem; `--no-baseline` leaves the baseline out.
  `evals/variants/drop.toml` turns compaction off; `evals/variants/no-sizing.toml`
  turns off context sizing (slice 2b), for a comparison with one binary;
  `evals/variants/verify.toml` turns on the verification gate (slice 1) with the
  workspace's unittest suite.
- `--window N` gives every model an N-token window (`[model.context_window]`),
  to run a large-window server as a small one. The plan's target is 32,768.
- `--jobs N` runs N at a time (default 1; a local server may handle more).
- `--agent86 PATH` is the binary; by default the newer of `target/release` and
  `target/debug`. The summary names the binary and its version.
- `--keep` leaves each run's workspace and home in the temporary directory.
- `--resume DIR` continues an earlier eval in that results directory: runs that
  finished with their provider reachable are kept, and only the missing or
  unreachable ones run. Give the same models, tasks, variants, and repeats.
- `--stop-after-unreachable N` (default 3; 0 never stops) stops handing out runs
  once N in a row could not reach their provider, rather than failing every run
  left against a server that is down; `--resume` continues from there.

A run whose provider could not be reached or broke off mid-stream (agent86's
"Cannot reach", "failed while streaming", HTTP 502 to 504, and the like) says
nothing about the model: it is printed as `DOWN`, recorded with `infra: true`,
counted in the summary's Unreachable column, and left out of every score.
Against a local server, start with `--jobs 1`: several runs at a time keep its
GPU at full load for as long as the eval lasts, far more than interactive use,
and that has been seen to take the serving machine down.

Each run gets a fresh copy of the task's workspace and a fresh isolated home
(`HOME` and `USERPROFILE`, every `AGENT86_*` variable of yours removed), whose
config is your providers, then [`evals/config.toml`](../evals/config.toml), then
the variant, then the run's own settings (the model, the task's step and time
budgets, `--window`). The agent runs as
`agent86 --model <ref> --approval auto run --json <goal>`: approvals are
automatic so writes and commands go ahead without a terminal, inside the sandbox
executor, in the throwaway workspace. Do not point a task at anything you would
not let an agent change.

Results go to `target/evals/<time>/` (or `--out`):

- `results.jsonl`: one line per run, written as each run ends (task, category,
  model, variant, repeat, passed, the agent's exit code, whether it timed out,
  steps, tool calls, tokens, cost, compactions, `context_trim` events, time, any
  error, whether the provider was unreachable (`infra`), the verification gate's
  status, runs, and failures sent back (`verify`, `verify_runs`,
  `verify_sent_back`, when the variant sets a gate), the end of the check's
  output, and the start of the final answer).
- `summary.md` (also printed): pass rate, median steps, tokens, cost, mean time,
  compactions, and trims per model and variant; then each task's passes per
  model and variant; then, when a variant sets the verification gate, what the
  gate did (runs checked, its verdicts, failures sent back, and how many of
  those runs then passed the task); then the errors.
- `runs/<model>/<variant>/<task>-<n>/`: the run's `config.toml`, `agent.json`
  (the `run --json` document), `agent.stderr`, `trace.jsonl`, and `check.txt`.

## Tasks

A task is a directory in [`evals/tasks/`](../evals/tasks):

| Path | What it is |
|---|---|
| `task.toml` | `goal` (word for word what the agent is asked), `category`, `measures` (which slice it is meant to show), `timeout_s` (default 600), `max_steps` (optional), `check` and `setup` (argument lists, no shell; `{python}` is the Python on PATH, `{setup}` the task's `setup/`) |
| `workspace/` | Copied into a fresh directory: all the agent sees |
| `setup/` | Scripts `setup` runs in that directory before the agent starts, for fixtures too large to keep in the repository (seeded, so the same every time). Never copied |
| `check/` | Copied over the workspace only after the agent finishes, so the agent can neither read nor change it; then `check` runs there. Exit 0 is a pass. `EVAL_FINAL_FILE` names a file holding the agent's final answer |
| `solution/` | A reference fix, copied over the workspace. `solution/eval_final.txt` stands in for the final answer; `solution/eval_delete.txt` lists paths (one per line) the fix removes |

Tasks are Python with the standard library only, so they run in seconds on every
platform; the checks are `unittest` suites in `check/eval_check/`. A task is
honest when its check fails on the untouched workspace (a do-nothing agent
scores nothing) and passes with the reference solution: `agent86-eval validate`
checks both, and `cargo test -p agent86-eval` (part of the workspace's tests, so
CI) runs it over every task, skipping when no Python is on PATH.

Twenty-four tasks, two to four for each kind of failure the later slices
address, in nine categories:

| Task | Category | Meant to show |
|---|---|---|
| `bugfix-pagination` | bugfix | The visible tests show one bug; a second is only in the docstrings |
| `bugfix-business-days` | bugfix | A rule the visible test names, broken again on a path it does not test |
| `bugfix-memoize` | bugfix | Two bugs in one small decorator, one of which no visible test shows |
| `feature-slugify` | feature | A function written to a precise spec |
| `feature-roman` | feature | A pair of functions to a strict spec, including what to refuse |
| `feature-cli-limit` | feature | Options added to a command-line tool, checked by running it |
| `feature-contact-phone` | feature | One field threaded through a model, its validation, and its stored form |
| `refactor-rename` | refactor | A rename across four files that must all land, with a deprecated alias |
| `refactor-split-module` | refactor | A module turned into a package without breaking an import |
| `long-context-chain` | long-context | Compaction (slice 2): twelve notes of about 1,500 tokens, linked so they must be read one at a time, with facts from the first needed at the end |
| `long-context-spec` | long-context | Compaction (slice 2): eight rules spread through a 33 KB spec, all of which the code must follow |
| `config-hunt` | search | Three settings to change among forty files, and seven look-alike values that must stay |
| `search-remove-flag` | search | Every use of a feature flag removed, keeping the right branch each time |
| `search-callers` | search | Callers of a function, at any depth, through direct, aliased, and module-attribute calls, past a look-alike name; scored against the call graph read from the code |
| `log-triage` | large-output | Tool output sized to the window (slice 2b): a 400 KB log whose one traceback is at character 280,000, past `read_file`'s 100,000-character cap |
| `large-output-noisy-tests` | large-output | The same for commands: the suite logs about 930 KB before unittest's failure report |
| `large-output-events` | large-output | A 4.8 MB event log, with timestamps whose offsets cross the month and a look-alike event type |
| `verify-csv` | verification | The verification gate (slice 1): edge cases a first attempt usually misses, with tests to run |
| `verify-intervals` | verification | Boundary cases (adjacent integers, bad input) the visible tests only partly cover |
| `verify-semver` | verification | Semantic Versioning precedence, with many corners and few visible tests |
| `stall-cwd` | stall | The stall nudge (slice 3): the error points at the settings file, which is fine, and the obvious fixes fail the same way |
| `stall-circular-import` | stall | A circular import that reordering the imports does not fix |
| `stall-money-float` | stall | Money in floats, where rounding them, the obvious fix, fails other cases |
| `tests-duration` | tests | Tests written by the agent, which must pass on the real code and catch six mutants, each breaking one documented rule |

## Adding a task

1. Make the directory, its `task.toml`, `workspace/`, `check/eval_check/`
   (with an empty `__init__.py`), and `solution/`.
2. Write the goal as a user would, and the check so it tests what the goal and
   the workspace's docstrings say, nothing they do not.
3. `cargo run -p agent86-eval -- validate --task <id>`.
4. Run it against a model once and read `check.txt` and `agent.json`: a failure
   should be the model's, never an ambiguity in the task.

## Not built yet

- Comparing two result directories (today, run the variants in one invocation).
- Killing the agent's child processes at the runner's time limit: only
  `agent86` itself is killed, and its own commands have their own timeouts.
