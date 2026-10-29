# agent86-rs Project Plan

**Last updated:** 2026-10-28  
**Current stage:** First milestone complete; the Ollama provider (stage 2) ported; every model provider ported (Anthropic and llama.cpp added); pricing, the system prompt, and keyring keys ported; `agent86 keys` added; read-only workspace tools (stage 3) and the ReAct loop (stage 4) ported; approvals, file-writing tools, guardrails (stage 5), sandboxed command and Python execution (stage 6), `web_fetch`, sessions, and the flight recorder (stage 7) done; the MCP client (stage 8) done; the plain interactive REPL with turn cancellation (stage 9, part 1) done; the interactive interface (stage 9, parts 2 to 6) done as an inline, Claude Code-style interface ([ADR 0001](adr/0001-inline-terminal-interface.md), [design](INTERFACE.md)): the inline base, the `/` and `@` menus and the pickers, Markdown, diffs, and the transcript view, model catalogs, providers, keys, and config write-back, and MCP management; episodic and semantic memory (stage 10, part 1) and the local sentence-transformers embedder (stage 10, part 2) done, so every stage of the roadmap has landed; context-window fitting and compaction done; skills and delegation (`delegate` and sub-agents) done; turn limits adjustable at runtime (`/limits`, `run` flags, and the interface's offer to raise a limit), a deliberate addition beyond Python; the remaining CLI commands (`models`, `config`, `skills`, `--sandbox`) done; triage routing and OpenTelemetry span export done; released as v1.0.0 (binaries for Linux, macOS, and Windows built by `.github/workflows/release.yml` from a `v*` tag, notes in `docs/releases/`)

## 1. Purpose

agent86-rs is an experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the Python agentic command-line harness.

The goal is not to translate every Python file mechanically. It is to build a coherent Rust implementation that preserves the existing behavioral contracts while exploring:

- Easier distribution of the core application as a native executable
- Fast startup and predictable resource usage
- Explicit, compiler-checked state transitions and provider-neutral types
- A unified async design for providers, tools, cancellation, and external processes
- A testable migration path that keeps the Python implementation as the behavioral reference

Performance and resource improvements should be measured rather than assumed. Model APIs, MCP servers, subprocesses, and I/O will remain the dominant latency sources for many workloads.

## 2. Repository and reference implementation

- **Rust implementation:** [tdfleming/agent-86-rs](https://github.com/tdfleming/agent-86-rs), currently private
- **Behavioral reference:** [Maetek-Technologies/agent-86](https://github.com/Maetek-Technologies/agent-86)

The Rust repository was created under the personal `tdfleming` account because organization OAuth restrictions prevented repository creation under `Maetek-Technologies`. It can be transferred later if desired.

The Python implementation remains the reference and compatibility oracle. This project does not currently replace it.

## 3. Current status

### Completed

- Created the private Rust repository
- Added a Cargo package and committed dependency lockfile
- Added provider-neutral core types:
  - Roles, agent phases, approval modes, and stop reasons
  - Model references and `provider:model` parsing
  - Tool specifications, calls, and results
  - Messages and completion requests, results, and streaming deltas
  - Steps and token/cost usage accounting
- Added JSON serialization and deserialization through Serde
- Added ten initial compatibility tests
- Added a placeholder `agent86` CLI supporting `--help` and `--version`
- Added the README, MIT license, `.gitignore`, and an implementation roadmap
- Added a cross-platform CI workflow for Linux, macOS, and Windows, testing on stable and the MSRV
- Raised the MSRV from 1.74 to 1.88: `thiserror` 2.x needs 1.77, and current `tokio` and `reqwest` (with their URL/IDNA dependencies) need 1.85–1.88
- Added the async `Provider` trait; `complete()` is derived from `stream()`, as in Python, so streamed and non-streamed text always agree
- Added the OpenAI-compatible provider, ported from `openai_provider.py`: request shaping, the sampling-parameter gate, the `max_completion_tokens` to `max_tokens` fallback, SSE decoding, tool-call fragment accumulation, usage, stop-reason normalization, split connect/read timeouts, and retries that never fire after text was emitted
- Added layered TOML configuration ported from `config.py` (defaults, user file, project file, `AGENT86_MODEL`, `--model`), limited to `[model]`, `[limits]`, and `[providers.*]`
- Added the `provider:model` factory: `openai`, and any provider block with a `base_url`, build the OpenAI-compatible provider; keys come from the environment variable named by `api_key_env`
- Added the one-shot CLI (`clap`): `agent86 [--model provider:model] run [--json] <goal>`, streaming text to stdout with the turn summary on stderr, or one JSON object with `session_id`, `output`, `steps`, `usage`, and `turn`; errors go to stderr with exit code 1, usage errors exit 2
- Added pricing ported from `pricing.py`: the built-in price table, local providers priced at zero, dated-snapshot prefix matching, prompt-cache rates, and `[pricing.models]` config overrides. The summary line shows `$0.0000`-style costs for priced models and `cost n/a (unpriced model)` otherwise, and `cost_usd` is filled in. As in Python, OpenRouter and Groq models stay unpriced unless overridden
- Matched Python's `json.dumps` float formatting for `cost_usd` and `duration_s` (`1.44e-05`, not `0.0000144`)
- Added the system prompt ported from `cognitive/prompt.py`: Python's identity text plus `Environment: agent86 v<version>. Default model: <ref>. Sandbox: <mode>.`, sent as the first message of every request. A fixture extracted from the Python source (by parsing, not executing it) keeps the text identical
- Added `[sandbox] mode` (`subprocess` or `docker`) and the `AGENT86_SANDBOX` override; the mode is only reported in the prompt, since no sandbox exists yet
- Added OS keyring lookup ported from `secrets.py`: the `api_key_env` variable wins, then the keyring entry with service `agent86` and the provider section name as the account (Windows Credential Manager, macOS Keychain, Secret Service on Linux). Windows lookups follow the Python `keyring` package's layout (`agent86`, then `<account>@agent86`), confirmed against its source, so keys Python stored are found. Any keyring failure degrades to environment-only
- Added `agent86 keys set <provider> [--stdin]` and `agent86 keys delete <provider>`, which write and delete the same keyring entries Python uses (on Windows, Python's `agent86` / `<account>@agent86` layout). The prompt hides input, including in Git Bash, where it uses `stty -echo` because console-based prompts (Python's `getpass`, `rpassword`) cannot read mintty input. Keys are checked before storing: empty input, embedded whitespace, and a key pasted more than once are refused. Verified live on Windows with a store, read, and delete round trip under a throwaway service name
- Added the tool layer and the read-only workspace tools (stage 3), ported from `tools/base.py`, `tools/registry.py`, `tools/sandbox/policy.py`, and `tools/builtin/files.py`: the `Tool` trait, pydantic-style argument validation with pydantic's messages, the workspace jail (`resolve_within`, following symlinks like `Path.resolve()`), output truncation, the registry (registration-order specs, sorted names, unknown-tool errors), and `read_file` and `list_dir`. Their advertised JSON schemas match a fixture generated with pydantic from mirrors of the Python argument models. 
- Added the ReAct loop (stage 4), ported from `orchestration/loop.py` and `orchestration/circuit.py`: each step sends the full history plus the tool specs; tool calls are announced as `[tool] name(<json.dumps args>)` and observed as `[tool] name -> <first line>`, both in the streamed output and in `run --json`'s `output`, as Python does; results return to the model in call order; read-only calls in one step run concurrently (`limits.parallel_tools`); unparseable tool-call JSON is answered before dispatch; the circuit breaker enforces `max_steps` (40), `max_cost_usd` (5.0), `max_wall_clock_s` (900), and `max_consecutive_errors` (3) with Python's messages; answers cut off by the output cap get up to 3 continuations, collapsed into one answer. `run` offers `read_file` and `list_dir`, jailed to the current directory. Tool-call arguments are sent back as Python's `json.dumps` output, and `serde_json`'s `preserve_order` keeps argument key order
- Added the approval gate and the file-writing tools (stage 5, part 1), ported from `guardrails/policy.py`, `ui/repl.py`, and `tools/builtin/files.py`: `[guardrails] approval` (`ask` by default, `auto`, `deny`) with `AGENT86_APPROVAL`, `--approval`, and `run --yes`/`-y`; a `[y/N]` terminal prompt that shows the tool's preview (a unified diff) and is installed only when stdin is a terminal, so piped runs decline side effects under `ask`; Python's batch ordering (approvals first, reads in parallel, then writes in call order) and its `Not executed: <reason>.` observations; `write_file` and `edit_file` with Python's messages, BOM and CRLF preservation, aliases (`old`/`new`), and pydantic's boolean rules. Diffs come from a port of CPython's `difflib` (`SequenceMatcher` with its autojunk heuristic, and `unified_diff`), checked against CPython output
- Added the guardrails (stage 5, part 2), ported from `guardrails/scanners.py`, `ingress.py`, and `egress.py`: Python's injection, secret, and PII regexes verbatim (checked against strings extracted from the Python source by parsing it) with results checked against CPython's `re`; `[guardrails] ingress` (`off`/`warn`/`block`), `egress` (`off`/`warn`/`redact`), and `scan_observations`. As in Python, a flagged goal is noted (`[guardrail] input flagged: ...`) or, in block mode with injection, refused without a model call; each answer is scanned and flagged after it, or in redact mode buffered and replayed redacted, including in the history; tool output with injection is wrapped in the untrusted-data banner; and `run --json` output gets a final egress pass
- Added sandboxed execution (stage 6), ported from `tools/sandbox/policy.py`, `subprocess_exec.py`, `docker_exec.py`, `executor.py`, and `builtin/shell.py` and `python_exec.py`: `run_command` (`cmd.exe /c` or `/bin/sh -c`, as Python's `shell=True`) and `python_exec` (code over stdin to `python -I -`), both side-effecting; the workspace as working directory; Python's environment allowlist, `LC_`/`XDG_` families, `sandbox.env_passthrough`, and credential-name refusal; `limits.tool_timeout_s` (60) with the child in its own process group and the whole tree killed on timeout (`taskkill /T` or a group `SIGKILL`), exit code 124 and `[timed out after Ns]`; text-mode decoding with universal newlines; and the opt-in Docker backend (`--rm`, `--memory`, `--cpus`, `--pids-limit 256`, `--network none` unless enabled, `/workspace` bind mount, named containers killed on timeout), falling back to the subprocess sandbox with Python's `sandbox:` note when Docker is unavailable
- Added `web_fetch`, ported from `tools/builtin/web.py`: read-only; http(s) only; every hop (the URL and each redirect, followed manually up to 5) resolved and refused if any address is unspecified, loopback, link-local, private, multicast, or reserved, with Python 3.12 `ipaddress` semantics including unwrapping IPv4-mapped, 6to4, and Teredo addresses (checked against stdlib `ipaddress`); connections pinned to the vetted addresses; a 2 MB body cap before decoding; textual content types only; charset decoding; Python's regex HTML reducer (checked against a mirror of it); `[tools] web_allow_private`, `web_user_agent`, and `web_max_chars` (8000)
- Added sessions (stage 7, part 1), ported from `orchestration/state.py`, the session half of `memory/store.py`, and `Harness.new_session`/`resume`/`_persist`: `AgentState` (messages, steps, cumulative usage, `last_turn`) threaded through the loop, which now records each model call as a `Step`; Python's SQLite schema and upsert in `[memory] path` (`~/.agent86/memory.db`), saved when a session starts and after every turn, including failed ones (phase `error`); write-once titles from Python's `session_title`; `run --session`/`-s` to resume, starting a new session for an unknown id as Python does; `run --json` `steps` and `usage` cumulative across the session. Interoperability was checked both ways: Rust reads a pydantic-shaped `state_json`, and a Rust-written session with tool calls validates against pydantic mirrors of Python's models
- Added the flight recorder (stage 7, part 2), ported from `observability/recorder.py`, `observability/redact.py`, and the `trace` commands in `cli.py`: Python's events and fields (`turn_start`, `guardrail` at the `ingress_input`, `egress`, `egress_tool_args`, and `observation` stages, `model_call`, `tool_call` for dispatched calls, `continuation`, and `turn_end` with `done`, `blocked`, or `error` and Python's reasons) written as `json.dumps` lines to `[observability] path`/`trace.jsonl`; every event redacted (the guardrail secret patterns and key shapes become `***REDACTED***`, and `arguments`, `task`, `content`, `error`, and `outcome` are clipped to `max_field_chars`); rotation at `max_trace_bytes` keeping `keep_traces` generations; `agent86 trace path`, `trace show` (`-s`, `-n`, `-k`, `--since`), and `trace export` (`jsonl`, `json`, `otlp-json`, `-o`), with the OTLP reconstruction matching Python's output byte for byte
- Added the MCP client over stdio (stage 8, part 1), ported from `tools/mcp_client.py`, `tools/mcp_content.py`, the `${VAR}` expansion in `secrets.py`, `render_mcp_section` in `cognitive/prompt.py`, and the `mcp` commands in `cli.py`: `[mcp] enabled` and `[mcp.servers.<name>]`/`[mcp_servers.<name>]` with Python's fields, defaults, and transport validation messages; a hand-written newline-delimited JSON-RPC client (`initialize` with protocol `2025-06-18`, `notifications/initialized`, paginated `tools/list`, `tools/call`, `ping` answered) for a server started with the scrubbed tool environment plus its own `env`, with `${VAR}` in `args`, `env`, `url`, and `headers` resolved from the environment and then the keyring; `connect_timeout_s`, `call_timeout_s`, and `max_result_chars` with Python's messages; start failures as notes, including the server's redacted stderr tail; tools named `mcp__<server>__<tool>` (sanitized, 64 characters), described `<description> (MCP: <server>)`, side-effecting unless `readOnlyHint`, with the argument schema deferred until first use (ADR 0003) and Python's missing-argument error; Python's content rendering (text, image, audio, embedded resources, resource links); name collisions skipped rather than shadowing; the MCP section of the system message; and `agent86 mcp list` and `agent86 mcp tools`. Tests run a stdlib-only Python MCP server fixture
- Completed the MCP client (stage 8, part 2): the streamable HTTP transport (POSTed messages answered as JSON or an SSE stream, the `Mcp-Session-Id` and `MCP-Protocol-Version` headers, the optional GET stream for server messages, and a session `DELETE` on close) and the legacy SSE transport (the `endpoint` event, same-origin check, responses on the stream), both with configured `headers`; protocol revision `2025-11-25`, as the Python SDK negotiates; a supervisor per server that, on a mid-session drop, unregisters the server's tools with Python's note (including the stderr tail), reconnects with Python's backoff (5 attempts, 0.5-30 s equal jitter), and re-registers them, with Python's unavailable, reconnecting, and give-up messages; `tools/list_changed` (and the resource and prompt variants) re-listing the tools mid-run, keeping unchanged tool objects and their materialised schemas; progress tokens on `tools/call` and throttled progress notes; resource-update notes; a bounded note log with listeners for a future REPL; the synthetic `list_resources`, `read_resource`, `list_resource_templates`, `subscribe_resource`, `unsubscribe_resource`, `list_prompts`, `get_prompt`, and `complete` tools, gated on the server's declared capabilities, with pydantic's schemas and validation messages (`tests/fixtures/mcp/capability_schemas.json`, from a pydantic mirror of the Python models) and Python's renderings; the MCP section of the system message rebuilt for every request; and the tool registry made shareable (lock-guarded, identity-checked removal) so the manager can update it during a run, as Python's is. As in Python, `tools/list` reads one page
- Added the interactive REPL (stage 9, part 1), ported from `ui/repl.py` (the plain loop), `ui/history.py`, `tui/mentions.py`, and the surface-shared registry in `tui/commands.py`: `agent86` with no subcommand, plus `--resume`/`-r` and `--plain`; the `agent86> ` prompt, the `agent86 ` answer label, dim harness notices, `(no response)`, `error:` lines that leave the loop running, the per-turn summary line, `bye` at end of input or `/exit`/`/quit`; Python's banner (a Rich rounded panel, checked against Rich's own rendering) when stdout is a terminal; startup notes (resume, memory, MCP, tool-name collisions, sandbox, session id) and MCP notes polled before each prompt; `/help` (checked against Rich), `/config`, `/config model`, `/config mcp`, `/models`, `/model`, `/tools` (with each disconnected MCP server's status), `/skills`, `/memory`, `/mode` (Python's ask, auto, deny cycle), `/cost`, `/sessions`, `/resume` (by id or unique prefix), and `/clear`, with Python's messages; the approval prompt on stdout at a terminal; `@path` mentions jailed to the workspace and inlined as fenced blocks (Python's pattern, size cap `[tools] mention_max_bytes`, binary refusal, directory listings, and refusals shown and sent), with the trace recording the typed line; the prompt history file (`[ui] history_file`, `history_size`) with bash's rules and Python's escaping, shared with Python; `[ui]` (including the legacy `status_line` key), `[model] router`, and `[model.route]` read. Turns can be cancelled, ported from `Harness.cancel()` and its safe points: before each model call, while a response streams (a stalled stream is woken), and between tool calls, never inside one; a cancelled turn shows `[cancelled]`, keeps the partial exchange, is saved in the `error` phase, and records `turn_end` with status and reason `cancelled`. In the REPL, Ctrl+C cancels the running turn and starts a fresh line at the prompt
- Added the inline interactive interface (stage 9, part 2; [ADR 0001](adr/0001-inline-terminal-interface.md), [design](INTERFACE.md)), the default for `agent86` at a terminal. It reworks Python's full-screen `tui/app.py` to render in Claude Code's style: the conversation goes into the terminal's own scrollback, and only a live region at the bottom is redrawn. It has:
  - a block-letter `AGENT 86` header in a cyan-to-violet gradient over the version and working directory (one line on a terminal too narrow for it; beyond Python, whose compact banner shows the version and model; the model is left to the status line, which follows `/model`), and the startup notes
  - a bordered multi-line input box: Enter sends, Shift+Enter or Ctrl+J adds a line, Up and Down walk the shared history from the edge lines, and Escape clears; it is disabled while a turn runs, as in Python
  - the user's line echoed as `> ...`
  - the reply streamed under a `⏺` marker, word-wrapped and committed a paragraph at a time (Python's 2,000-character tail limit)
  - harness notices, dim, on their own line
  - each tool call shown while it runs, then committed as `⏺ name(args)` and `⎿ summary` when its result lands (Python's 72- and 100-character previews; red when the call failed)
  - a spinner line with the phase (`thinking`, `running <tool>`, `cancelling`) and the elapsed time
  - Python's status line (model, context gauge, tokens, cost, sandbox, approval mode, `[Shift+Tab]`), which drops the hint, then the tokens, then the gauge as the terminal narrows
  - the working directory (beyond Python, home shown as `~`) set into the input box's top border, right-aligned, so it is always on screen without competing with the status line; a narrow box shows its last component, then none
  - side-effecting calls asked in an approval panel in place of the input box: the tool name, the argument summary, and the tool's detail, such as the diff; answered with `y`, `n`, Escape, or the arrows and Enter
  - Escape or Ctrl+C cancelling a running turn at its next safe point, and Shift+Tab cycling the approval mode, even mid-turn
  - MCP notes shown as they arrive, Python's error line with the `see agent86 trace show -s <id>` hint, and the turn summary
  - slash commands run through the same registry as the plain loop; `/clear` clears the screen
  - Python's surface selection: `--plain`, `AGENT86_PLAIN`, `[ui] tui = false`, or a non-terminal stdin or stdout selects the plain loop. A terminal that can't host the interface falls back to it with Python's `TUI could not start (...)` note; the main case is Git Bash's mintty, whose pipes are not consoles.
  - The harness now reports structured turn events (text, tool call, tool result) alongside the text it streams.
  - The status line's context window comes from a port of Python's `context_window_for`, with `[model.context_window]` overrides.
- Added the interface's menus and pickers (stage 9, part 3), ported from the palette and picker code in `tui/app.py`, `tui/mentions.py`'s `complete_mentions`, and `tui/screens/mode_picker.py`, `model_picker.py`, and `session_picker.py`:
  - the `/` command menu under the input box, shown while the draft starts with `/` and has no space (Python's rule), listing each matching command with its description; Up and Down move, Enter runs the highlighted command, Tab writes it out for an argument, and Escape closes the menu
  - `@` path completion for the mention under the cursor: up to 20 workspace paths, directories first with `/`, noise directories and dotfiles hidden unless named, never outside the workspace, quoted when the path has a space; choosing a directory keeps completing inside it
  - the registry's `needs_choice` (Python's `ChoiceKind`): a bare `/mode`, `/model`, or `/resume`, typed or picked from the menu, opens a picker panel in place of the input box. The mode picker starts on the current mode; the model picker lists the configured role slots (`default`, `route.cheap`, `route.frontier`), deduplicated with their roles named together, and prefills `/model ` when none is set; the session picker lists recent sessions with Python's `title · id · age` labels, filtered by typing. A choice is dispatched as `/mode <mode>` or `/model <ref>`, as in Python
  - picking a session rebuilds the screen, as Python's `load_session` does: the banner, the saved conversation (prompts, replies, and each tool call with the first line of its result), then `resumed session <id> (<n> messages) - <title>`
  - `/clear` is recognised by the command result's `clears_transcript` flag, as in Python, not by its name
- Added rich rendering to the interface (stage 9, part 4), ported from `tui/widgets/transcript.py`, `tool_block.py`, and `tui/screens/approval.py`:
  - replies rendered as Markdown with `[ui] markdown` (on by default, as in Python): headings, emphasis, inline code, bullet and numbered lists (nested), block quotes, tables, rules, links shown with their address, and fenced code with a gutter. A block is committed once it is complete, so a code fence or a loose list is never split; a block longer than Python's 2,000-character tail limit is cut at a newline, and a code fence cut that way is closed and reopened
  - the tools' `preview_lexer` (`diff` for `write_file` and `edit_file`, `bash` for `run_command`, `python` for `python_exec`), carried on the approval preview; the approval panel colours a diff (additions, removals, hunk headers)
  - the Ctrl+O transcript view: the session's transcript on the alternate screen, with every tool call expanded as Python's `body_lines` (the arguments as indented JSON, the result capped at 200 lines); `a` or Ctrl+Shift+O collapses or expands them all, the arrows, PgUp/PgDn, Home, and End scroll, and Escape, `q`, or Ctrl+O returns with the screen as it was. It can be opened during a turn, and an approval request closes it
  - the decision not to add a syntax highlighter for now (see [design](INTERFACE.md), section 6)
- Added model and provider management to the interface (stage 9, part 5), ported from `cognitive/catalog.py`, `cognitive/remote_windows.py`, `config_writer.py`, `tui/screens/provider_manager.py`, `key_entry.py`, `connection_test.py`, `save_diff.py`, `model_picker.py`, and the chains in `tui/app.py`:
  - live model catalogs in all three of Python's shapes (OpenAI-compatible `/v1/models`, Anthropic, and Ollama's `/api/tags`; llama.cpp reports none), with Python's messages for every failure, fetched on a worker and cached per provider for the session. A bare `/model` opens the picker once the active provider's catalog is in: the role slots, then the catalog without repeats
  - a typed `/model <id>` that fails and looks like a bare model id is retried as `<active provider>:<id>` when that provider's catalog lists it exactly (`resolved to …`), with Python's queueing while the catalog is fetched; a ref naming a real provider gets its own error at once
  - `/config model`: the provider list in config order with each key's status (never the key), masked key entry when a provider has none, the catalog picker (type to filter; Python's free-text fallback when nothing or several match, or there is no catalog), a live one-token connection test capped at 15 seconds with "Save anyway" after a failure, then the save panel. A typed key is held in memory and stored in the keyring only after the test passes or the user saves anyway; a passing test switches the model at once
  - comment-preserving config write-back (`toml_edit`): `plan_edit` and `apply_edit`, the user and project scopes, Python's secret guard (secret-shaped keys only as pure `${VAR}` references), the unified diff shown before anything is written, atomic writes, and a reload from the files and the environment
  - the status line's context window from OpenRouter's own `context_length`, memoized and cached for 24 hours in `~/.agent86/cache/model_context_windows.json` (Python's file), with a 3-second fetch; every failure falls back to the table
  - providers are listed in config order everywhere (`/models` too), as in Python; they were sorted by name before
  - the plain loop's `/config model` now gives Python's wording
- Added the Ollama provider (stage 2), ported from `cognitive/ollama_provider.py`: `ollama:<model>` streams from `/api/chat` as NDJSON with Python's payload (`options.temperature`, `num_ctx` from `[providers.ollama]`, and `num_predict` only when a cap is set), tools passed through, tool results carrying the tool's name, tool calls numbered `ollama-0`, `ollama-1`, ... with arguments taken as an object or a JSON string, usage from `prompt_eval_count` and `eval_count` at zero cost, Python's `done_reason` mapping (`load` and `unload` are `other`; `stop` with tool calls is `tool_use`), the shared retry rules (never after text), and Python's messages for an unreachable server, both timeouts (naming `[providers.ollama]`), HTTP errors, `error` lines, malformed NDJSON, and a stream cut short. It needs no key. Live check (2026-10-27, local Ollama, `gemma4:12b`): `run` called `list_dir` and named both files, at `$0.0000`
- Added delegation, ported from `tools/builtin/delegate.py`, `agents/subagent.py`, and the harness's `spawn_subagent` and sub-agent usage accounting in `orchestration/loop.py`:
  - `delegate` (on by default, `[agents] enabled`; Python's schema; read-only, and never in a parallel batch, as Python's `parallel_safe = False`) runs a role-scoped sub-agent and returns its answer as the tool result
  - the sub-agent gets Python's persona (`You are a '<role>' sub-agent ...`) over the parent's system content, the task as its only message, the same tools, approval gate, guardrails, and recorder, its own skill state, and its own step budget (`[agents] max_steps`, 8, never above `[limits] max_steps`); its requests are fitted to the window like the parent's
  - depth is bounded by `[agents] max_depth` (2): at the limit `delegate` is not offered, and a call beyond it gets `[delegation refused: max agent depth N reached]`; a tripped budget answers `[sub-agent '<role>' halted: ...]`, a failed model call fails the delegate call as `ProviderError: ...`, and invalid arguments get pydantic's message
  - what a sub-agent spends (and anything it delegated) is added to the delegating step, the turn summary, the session usage, and the cost cap, without counting as the parent's steps
  - Python's trace events: `spawn` (session `sub`), and per sub-agent (`sub:<role>`) `model_call` with `agent` and `depth`, `tool_call` with `depth`, observation `guardrail`, and `subagent_done`
  - live check (2026-10-27, local Ollama, `gemma4:12b`, throwaway workspace and home): asked to delegate a review, the model called `delegate` with role `reviewer`; the sub-agent listed and read the files in four steps, and the parent summarized its report
- Added OpenTelemetry span export, ported from `observability/tracing.py` and the spans `orchestration/loop.py` opens: with `[observability] otel = true`, each user turn is a `turn` span (`session.id`, `gen_ai.request.model`, then the turn summary's tokens, cost, steps, tool calls, duration, and phase), parent of a `model_call` span per main-loop model call (`gen_ai.system`, `gen_ai.request.model`, `agent86.step`, then `agent86.completed`, usage, `gen_ai.response.finish_reasons`, cost, and the tool-call count) and a `tool_call` span per dispatched call, `delegate` included (`tool.name`, `gen_ai.tool.name`, `tool.ok`); sub-agent calls and compaction summaries get none, as in Python. A span an error escapes gets the ERROR status (`<Exception>: <message>`) and an `exception` event, as the SDK records one. `otel_exporter` is `otlp` (the default), `console` (each span as the SDK's `to_json()` on stderr), or `none`; `otel_endpoint`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, and `OTEL_EXPORTER_OTLP_TIMEOUT` are honoured. Spans are batched on a worker thread and flushed when a turn ends and at exit, and a collector that cannot be reached is kept as a note, never an error
- Added triage routing, ported from `orchestration/router.py` and its use in `orchestration/loop.py`: with `[model] router = "triage"`, each turn goes to `[model.route] cheap` when the task is at most 240 characters (after stripping) and has none of Python's complexity keywords (`code`, `debug`, `fix`, `review`, `step by step`, and the rest, whole words, any case), else to `[model.route] frontier`. The route is chosen after the ingress guardrail lets the input through, from the text the model sees (with `@file` mentions inlined), and recorded as Python's `route` event (`model`). Routed providers are built on first use from the config the session started with and cached by ref; the request's output cap and the context window follow the routed model, as do sub-agents, the status line, `/model`'s report, and the pricing of the turn summary. Compaction summaries go to the cheap route (or the active model when it cannot be built). `--model` sets the default and leaves routing on, as in Python; `/model` pins the chosen provider, which turns routing off for the rest of the session and drops the cached providers
- Added the remaining CLI commands, ported from `cli.py`: `agent86 models` and a bare `agent86 config` (Python's `_list_models`: the providers with each key's source, never the key, the keyring line, then the model roles and whether each ref parses), `agent86 config path` (each layer's absolute path and whether it exists), `agent86 config show` (the resolved configuration as JSON), `agent86 skills list` (the skills table, or `No skills found. Searched: ...`), `agent86 skills show <name>` (the instructions and bundled resources, or Python's error naming the known skills, exit 1), and the global `--sandbox`, which overrides `AGENT86_SANDBOX` and `[sandbox] mode`. The hints that name these commands now use Python's wording: the model-error hint (`run` and the REPL's `Cannot start:`), the malformed-config hint, `Memory is disabled in config.`, the memory database error, and `mcp list`'s empty hint
- Added turn limits at runtime, a deliberate addition (Python reads `[limits] max_steps`, `[limits] max_output_tokens`, the provider's `max_tokens`, and `[agents] max_steps` only from config, and offers no way to change them while it runs). It came out of a delegated review on a thinking model whose sub-agents spent their whole 8192-token output cap reasoning:
  - three limits can be overridden for the rest of the process: `steps` (the turn's step budget), `agent_steps` (each sub-agent's, still never above `steps`), and `output` (the output cap of every model call). The `output` override replaces whichever setting Python's `max_output_tokens_for` picks (the provider's `max_tokens`, else `[limits] max_output_tokens`), so it always takes effect; overrides survive `/model`, `/clear`, and `/resume`
  - `/limits` (both surfaces) shows each limit with the setting it comes from, `/limits <steps|agent_steps|output> <n>` overrides one, and `/limits reset` drops the overrides. In the interface, `/limits save` shows the diff in the save panel and writes each override to the setting it replaces (the provider's `max_tokens` when that section sets one), comments kept; the plain loop points at the interface
  - `run --max-steps`, `--max-output-tokens`, and `--max-agent-steps` set the same overrides for one run
  - in the interface, when the turn or a sub-agent reaches its step budget, or a reply reaches the output cap before writing any text, a panel offers to raise the limit to double or four times it and goes on: the breaker gets the new budget, or the request is sent again with the larger cap (the empty reply stays out of the history, and the call still counts as a step). Stopping, or `run` and the plain loop, which have no offer, end the turn or the sub-agent as before. Each raise is traced as a `limit_raised` event (`limit`, `agent`, `from`, `to`)
  - the sub-agent's no-answer notice names the setting to raise (the provider's `max_tokens`, `[limits] max_output_tokens`, or the session's override); it named `[limits] max_output_tokens` even where that setting had no effect
- Added skills, ported from `skills/` (the loader and `Skill`), `tools/builtin/skills_tool.py`, the skills section of `cognitive/prompt.py`, the `allowed-tools` enforcement in `tools/registry.py` and `tools/base.py`, the skill roots in `tools/sandbox/policy.py`, and `/skills`:
  - discovery from `<workspace>/.agent86/skills`, `<workspace>/.claude/skills`, `~/.agent86/skills`, `~/.claude/skills`, then `[skills] paths`, each root's folders in Python's sorted order (case-insensitive on Windows), the first root winning a name; `[skills] enabled = false` finds none
  - `SKILL.md` frontmatter read with Python's built-in parser (`key: value`, quotes, inline and block lists, `>`/`|` block scalars with indicators, comments), the closing `---` only on its own line, a missing `name` taken from the folder, and `allowed-tools` in any spelling (spaces, commas, an inline or block list)
  - the system prompt's `Available skills` section (one line each, with `[tools while active: ...]`), and `use_skill` (registered only when a skill exists, Python's schema): the instructions, read once and kept, the bundled resources line, and the restriction notice, with Python's metadata and error for an unknown skill
  - while a skill with `allowed-tools` is active, every other tool except `use_skill` is refused with Python's message naming the skill and its list; the restriction lasts until the turn ends, however it ends
  - the skill roots join the sandbox's allowed roots, so a skill's bundled files outside the workspace can be read
  - `/skills` lists `name - description`
  - live check (2026-10-27, local Ollama, `gemma4:12b`, throwaway workspace and home): asked to use a project skill, the model called `use_skill`, got its instructions, and read the file they named with the one tool the skill allows
- Added context-window fitting and compaction, ported from `memory/working.py` and the compaction path in `orchestration/loop.py`:
  - every request is fitted to the model's real window: the window (`context_window_for`, with OpenRouter's own figure) minus the system message and the tool catalogue, minus `[limits] context_reserve_tokens` (4096) and the output cap (together clamped to half the window), floored at 1024 tokens, then capped by `[limits] max_context_tokens` (0, no cap); tokens are Python's estimate (code points over 4, counting tool names and arguments); the oldest messages are dropped first and a leading tool result never survives without its call
  - before each model call, a history over budget has its oldest span (never the last six messages, the current turn, or half of a tool block) replaced by a summary the model writes under Python's instructions (`GOAL`, `DECISIONS`, `FACTS`, `OPEN`), merged into the next user message as `[Conversation summary — earlier turns compacted]`; the turn streams `[compacted N messages into a summary]`, the summary call's usage and cost count toward the turn, the trace gets Python's `compaction` event, the replaced messages are archived as a `kind = "compaction"` episode (as Python's pydantic dump), and the session is saved at once
  - a summary that fails or comes back empty falls back to dropping, with `[compaction failed; dropped N messages]` and a `failed` or `empty` event; `[limits] compaction = "drop"` only drops
  - live check (2026-10-27, local Ollama, `gemma4:12b`, isolated home, `max_context_tokens = 120`): on the sixth turn of a session, five messages were compacted into an 84-token summary that kept every path, branch, version, and job name exactly, and the model answered from it
- Added the local sentence-transformers embedder (stage 10, part 2), Python's default `sentence-transformers:all-MiniLM-L6-v2`, ported from `SentenceTransformerEmbedder` and the cache rules in `memory/embeddings.py`, on candle instead of torch:
  - the model: BERT in `float32` (embeddings, attention, exact-`erf` GELU, layer norms as transformers' `BertModel`), the model's pooling (mean, CLS, max, or mean over the square root of the length), the Normalize module and then `normalize_embeddings=True`; the tokenizer is `tokenizer.json` through the `tokenizers` crate (the library Python's tokenizers wraps, at the same version), truncated to the model's `max_seq_length` (256 for MiniLM, from `sentence_bert_config.json`; 5.x's layout falls back to the tokenizer's limit) and never padded, as one text is encoded at a time
  - vectors agree with sentence-transformers' to about 3e-7 per component (two or three `float32` ulps: the kernels sum in a different order) and token ids exactly, for the real model and for a tiny random BERT fixture in the same layout; episodes Python embedded with MiniLM are now recalled by Rust with the same scores, and the reverse
  - the Hugging Face cache as `huggingface_hub` lays it out (`HF_HUB_CACHE`, `HF_HOME/hub`, or `~/.cache/huggingface/hub`; `refs/main` and `snapshots/<commit>/`): a cached model is loaded without a network call under `hf_offline` (Python's directory check, also honouring `HF_HUB_OFFLINE`); otherwise `main` is resolved at `HF_ENDPOINT` and the files sentence-transformers reads are downloaded (atomically, never the ONNX, OpenVINO, or PyTorch duplicates), falling back to the cache when the hub cannot be reached
  - a model that cannot load falls back to `hash:256` with Python's note, `sentence-transformers unavailable (OSError); using the offline hash embedder.` (`ValueError` for a model this cannot run)
  - a local model directory works as the spec, as in Python
  - live checks (2026-10-27): the default spec loads from the real cache in about 0.6 s with no note; on a copy of the real database, `memory search` ranked the episodes Python wrote with the model exactly as an independent sentence-transformers computation did, to two decimals; a download into an empty cache took 2.4 s, fetched the same commit and the same eleven files Python's cache holds, and sentence-transformers then loaded that cache offline and produced identical vectors; with the local Ollama (`gemma4:12b`, isolated home), `remember` and then `recall` in a new session found the fact at `[score 0.64]`
- Added long-term memory (stage 10, part 1), ported from `memory/store.py` (episodes, facts, ranking, pruning, retention), `episodic.py`, `semantic.py`, `system.py`, `embeddings.py` (the hashing embedder), `tools/builtin/memory.py`, the memory wiring in `orchestration/loop.py`, and the `agent86 memory` commands in `cli.py`:
  - episodes: every finished turn is recorded (task and answer) in the shared database; before a turn starts, past episodes scoring at least 0.35 (compactions excluded) are added to the system message as Python's "Relevant past experience" note, the task quoted as Python's `repr` and the outcome collapsed and cut at 200 characters
  - facts: `remember` and `recall` (not side-effecting, registered after the built-ins only when memory is on; `Memory is disabled.` otherwise), with Python's schemas, a `k` clamped to 1..20 and parsed with pydantic's lax `int` rules, the 0.30 floor, and `- <fact>  [score 0.62]` lines
  - the store: `float32` BLOBs as Python's `array("f")` writes them, a brute-force cosine scan (summed as Python 3.12's compensated `sum`, so scores are bit-identical), rows of another dimension skipped, and a stable sort
  - the hashing embedder (`hash:<dim>`), bit-identical to Python's (MD5 buckets and signs over `[a-z0-9]+` tokens), so facts and episodes either implementation writes with it are found by the other
  - retention at startup from `[memory] retention_max_episodes` (1000), `retention_max_sessions` (500), and `retention_max_age_days` (0, off), recorded as Python's `retention_prune` trace event; curated facts are never pruned automatically
  - `agent86 memory stats|sessions|search|prune|forget`, with Python's output, the dry run, and Click's `[y/N]` confirmation (`Aborted!` at end of input)
  - live check (2026-10-27, local Ollama, `gemma4:12b`, isolated home): asked to remember a preference, the model called `remember`; in a new session it called `recall` and answered from the fact
- Added the remaining model providers, so every adapter Python has is ported:
  - Anthropic, ported from `cognitive/anthropic_provider.py`: the Messages API event stream (`message_start`, content blocks, `input_json_delta` tool input, `message_delta` usage), the system prompt split out, tool results as `tool_result` blocks and tool calls as `tool_use` blocks, `max_tokens` always sent (the request's, then the section's, then 8192), `temperature` left out for models that removed it and dropped and remembered for the session when the API refuses it, prompt-cache markers on the tools and the system prompt when the prefix reaches the model's floor (`prompt_cache`, on by default), usage counting cached tokens as part of the prompt and pricing them at their own rates, and Python's stop-reason mapping. Python runs it through the `anthropic` SDK, so the SDK's behavior is reproduced too: its retries (408, 409, 429, and 5xx, `x-should-retry`, `retry-after-ms` or `Retry-After` up to a minute, otherwise 0.5 seconds doubling to 8), its fixed timeouts (5 seconds to connect, 600 between chunks), and its error text (`Anthropic API error: Error code: 401 - {...}` with the body as Python prints a dict, `Connection error.`, the timeout text, and `error` events). Failures the SDK leaves unconverted get the message Python's loop gives them
  - llama.cpp and LM Studio, ported from `cognitive/llamacpp_provider.py`: the OpenAI-compatible adapter under the name `llamacpp`, keyless, with `http://localhost:8080/v1` when the section has no `base_url`. Live check (2026-10-27): pointed at a local Ollama's OpenAI-compatible endpoint, `run` with `gemma4:12b` called `list_dir` and named the files, at `$0.0000`
  - the session-scoped memory of models that refused a sampling parameter, from `capabilities.py`, which the OpenAI-compatible adapter now consults too
- Added MCP server management to the interface (stage 9, part 6), ported from `tui/screens/mcp_manager.py`, `mcp_test.py`, the MCP chain in `tui/app.py`, and the harness's `ensure_mcp`, `add_mcp_server`, and `remove_mcp_server`:
  - `/config mcp`: the configured servers (config facts only, so opening it starts nothing) with Enter to edit, `d` to remove, and `t` to enable or disable, plus "Add from JSON" and "Add manually"
  - the form: a pasted JSON block in either README shape (a bare server, or `{"mcpServers": {...}}` / `{"servers": {...}}`, with `type` and `disabled` understood), or the fields (the command line split as Python's `shlex` does on each platform, `KEY=value` lists for env and headers, the URL, and http or sse). The config model's own message shows as it validates, and Continue waits until the server parses
  - a masked prompt for each `${VAR}` that neither the environment nor the keyring supplies; a typed value is used for the test and stored in the keyring under the variable's name only after the test passes or the user saves anyway, while the config keeps the reference
  - the MCP test: the server is really started (30-second budget) and its tools are listed, with "Save anyway" after a failure; then the save panel, and the tools are mounted at once, with name collisions reported. A cancelled add stops the server its test started
  - removing and disabling go through the save panel and unmount the server's tools at once; re-enabling is tested like an add
  - the config writer removes tables (comments written above a removed table stay in the file, as with Python's `tomlkit`) and writes lists and booleans
  - `McpManager` gained `start_server`, `mount`, and `remove_server`, and an empty manager is made for the first server added mid-session
  - the plain loop's `/config mcp`, the disconnected-server message, and `mcp list`'s empty hint now point at `/config mcp`, as Python's do; a run of stray spaces in two `agent86 mcp` messages is gone
- Added secret redaction ported from `secrets.py`, applied to error bodies and transport details
- Added recorded SSE fixtures and a scripted local mock HTTP server covering success, tool calls, empty, unterminated, malformed, truncated, stalled, unreachable, client-error, retry, and retry-exhaustion cases

Local validation completed successfully:

- `cargo test --all-targets`: **464 passed on Windows** (204 unit, 10 compatibility, 18 provider integration, 7 Ollama provider, 9 Anthropic provider, 3 catalog, 50 CLI end-to-end, 14 REPL end-to-end, 5 sessions, 8 trace, 18 MCP, 11 memory, 4 skills, 9 delegation, 6 embedding model, 13 tools, 11 write tools and approvals, 11 exec tools, 11 web fetch, 2 difflib, 2 guardrails, 34 agent loop, 2 system prompt, 2 API key; some exec tests are platform-specific), plus 2 opt-in live keyring tests and an opt-in check of the cached `all-MiniLM-L6-v2` (`AGENT86_TEST_MINILM=1`)
- `cargo fmt --all -- --check`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- CLI `run` and `run --json` against the mock provider: passed
- Live REPL check (2026-10-26, `openrouter:openai/gpt-4o-mini`, piped input): a turn mentioning `@shopping.txt` sent the file inline (the saved session holds the fenced block, titled with the typed text), the model answered with three items, and `/cost` and `/tools` reported the session. A manual check at a real Windows terminal (2026-10-26) passed for the banner, the approval prompt, and Ctrl+C cancelling a running turn
- Live MCP-over-HTTP check (2026-10-26, `openrouter:openai/gpt-4o-mini`): with the test server in streamable HTTP mode, `mcp tools` listed its tools, and in a `run` the model called `get_prompt` and described the prompt, including its non-text block; the server logged a session `DELETE` from each agent86 process
- Live MCP check (2026-10-26, `openrouter:openai/gpt-4o-mini`): with the test MCP server configured, `mcp tools` listed its eight tools; in a `run`, the model first called `mcp__test__add` with no arguments (its schema was still deferred), got the missing-argument error, called it again with both numbers, and reported 5555 (3 steps)
- Live flight-recorder check (2026-10-26): after a `run` that listed a directory, `trace show` listed the turn's five events with local times, token counts, and details
- Live session check (2026-10-26, `openrouter:openai/gpt-4o-mini`): a turn that read a file was saved; its `state_json` validated against pydantic mirrors of Python's models; and `run -s <id>` resumed it, with the model recalling the file's last item without tools
- Live `web_fetch` check (2026-10-26, `openrouter:openai/gpt-4o-mini`): the model fetched https://example.com and reported its heading, and a fetch of `http://169.254.169.254/latest/meta-data/` was refused before any connection
- Live sandbox check (2026-10-26, `openrouter:openai/gpt-4o-mini`, `run --yes`): the model ran `python_exec` (sum of squares, 385) and `run_command` (`ls`) in one step and reported both correctly
- Live file-writing check (2026-10-26, `openrouter:openai/gpt-4o-mini`, `run --yes` in a throwaway directory): the model called `edit_file` and `write_file` in one step, and both files on disk were correct
- Live agent-loop check (2026-10-26, `openrouter:openai/gpt-4o-mini`): asked to inspect this repository, the model called `list_dir`, then `read_file README.md` and `list_dir tests` in one parallel step, and answered correctly (3 steps, 3 tools, about 2.7k input tokens)
- Live keyring check (2026-10-26): with no environment variable set, `run` used the OpenRouter key from Windows Credential Manager and sent the system prompt (337 input tokens)
- Live OpenRouter smoke test (2026-10-26, `openrouter:openai/gpt-4o-mini`): `run` streamed the answer with the summary line on stderr, and `run --json` produced the contract fields with real token usage (12 in / 7 out). The key was read from the environment and appeared in no output

### Not implemented

The agent works end to end, but it does not yet have:

- Config sections other than `[model]`, `[limits]`, `[sandbox]`, `[guardrails]`, `[tools]`, `[memory]`, `[observability]`, `[mcp]`/`[mcp_servers]`, `[ui]`, `[pricing]`, and `[providers.*]` (others are accepted and ignored, not validated)
- Python's programmatic `SupervisorOrchestrator` (`fan_out` over a `MessageBus`); nothing in the CLI or the interfaces reaches it

### Known `run` differences from Python

- The system prompt is the identity, the environment line, the skills section, the MCP section when servers are live, and the turn's episodic recall note, as in Python.
- The environment line reports the Rust crate version. From 1.0.0 it matches Python's (`agent86 v1.0.0`), but the two are versioned independently.
- Usage is priced under the full `provider:model` ref rather than the bare model id. Python prices with the bare id, so a `[pricing.models]` override keyed by the ref (for example `"openrouter:openai/gpt-4o-mini"`) marks the model as priced but never applies the rate, and the summary shows `$0.0000`. The ref is tried first and then the bare id, so every other case matches Python. Python should get the same fix.
- `agent86 keys` has no Python CLI equivalent; Python stores keys only from its interactive `/config model` screen (which the interface here also has). Re-saving an account on Windows does not leave Python's stale `<account>@agent86` copy of the old key behind.
- The missing-key error names the keyring entry to create (service `agent86`, account `<provider>`) instead of pointing at `/config model`.
- Reading keys that Python stored is verified on Windows against the `keyring` package's source, and live on 2026-10-26: a key saved with Python's `keyring set agent86 openrouter` was read by Rust and accepted by OpenRouter. On macOS and Linux the attribute layouts match by inspection but have not been tested against entries Python wrote. `tests/os_keyring.rs` is an opt-in check for any platform.
- `agent86 models`, `agent86 config`, `config path`, and `skills list` print plain aligned tables with Python's columns (Rich tables in Python), and `models` always reports the OS keyring as available. `config show` prints the merged config layers as JSON (built-in defaults for the modelled sections, the files, and the overrides), not Python's full pydantic dump with every default, as `/config` does.
- Rich wraps stderr at 80 columns when it is not a terminal; Rust does not wrap. stdout, including `--json` output (2-space indent, non-ASCII unescaped), is unaffected.

### Known REPL differences from Python

- These apply to the plain loop (`--plain`, piped input, or a terminal that cannot host the interface). The interface's own differences are listed in the next section.
- Ctrl+C during a turn cancels it at the next safe point (`[cancelled]`, saved, traced as cancelled), which is what Python's full-screen app does with its cancel key. Python's plain loop instead raises `KeyboardInterrupt`, prints `interrupted`, and abandons the turn without closing or saving it. A running tool is never interrupted: the cancel takes effect when it finishes.
- Ctrl+C at an approval prompt does not decline; the prompt waits for an answer and the turn stops before the next call. Python declines.
- A cancel that lands after the model asked for tools but before any ran answers each call with `Not executed: cancelled.`; Python leaves those calls unanswered, which providers reject on the next request.
- `/config` prints the merged TOML layers (built-in defaults for the modelled sections, the config files, and overrides) as JSON, not Python's full pydantic dump with every default.
- `/config model` and `/config mcp` give Python's wording, pointing at the interface.
- `/models` always reports the OS keyring as available; Python checks for a usable backend. Its tables, `/sessions`, and the help list are plain aligned text (Rich tables in Python, with the same columns), and nothing is wrapped to the terminal width.
- `/model` switches the provider for later turns and pins it, turning triage routing off, as in Python.
- Answers are printed as plain text, as in Python's plain loop; the Markdown rendering behind `[ui] markdown` belongs to the full-screen app.

### Known interface differences from Python

- **Inline, not full-screen** ([ADR 0001](adr/0001-inline-terminal-interface.md)): the interface renders in Claude Code's style rather than as Python's full-screen Textual app, though the text of every message, note, error, and summary is Python's. Differences that follow from that:
  - The conversation stays in the terminal's scrollback after exit.
  - Replies and tool calls are marked `⏺` and `⎿`.
  - The prompt is a bordered box.
  - The approval request is a numbered panel, not a modal with buttons.
- **Running tool calls:** a running tool call is shown in the live region until its result lands; Python shows nothing for it until then.
- **Ctrl+C and Ctrl+D:**
  - When idle, Ctrl+C clears a draft. On an empty prompt it asks `Press Ctrl+C again to exit` for two seconds; Python quits on the first press.
  - Ctrl+D on an empty prompt quits.
  - Ctrl+C while an approval panel is open declines the call and cancels the turn. Python cancels the turn and leaves the modal open until it is answered.
- **Pasting:** where the terminal delivers a paste as keystrokes (Windows), an Enter with more keys already waiting behind it counts as part of the paste. It adds a line instead of sending the draft.
- **Menus and pickers:**
  - A command typed in full is the one highlighted in the `/` menu, so `/mode` and Enter runs `/mode`. Python highlights the first match, which there is `/models`.
  - Tab in the `/` menu writes the command out, followed by a space, for its argument; Python's Tab moves focus away from the prompt.
  - Choosing a file in the `@` menu closes it; Python reopens the menu on the completed path.
  - The session picker has one box: typing filters and Enter picks the highlighted session. Python's filter box picks on Enter only when exactly one session matches.
  - The mode and model pickers also take the choice's number. Ctrl+C closes a picker, as Escape does.
  - A command run from the `/` menu goes through the same path as a typed one: it is echoed (Python doesn't echo a bare `/mode`, `/model`, or `/resume` picked from its palette) and the full command goes into the prompt history (Python records what had been typed, such as `/he`).
- **Rendering:**
  - A reply is rendered as Markdown a block at a time as it streams, not as one document when it ends; a block is only committed once complete, so the result is the same except where Markdown links a later block to an earlier one (reference-style link definitions).
  - The look is our own, not Rich's: `⏺` before the reply rather than an `agent86` label, `•` bullets, a gutter for code, a box-drawn rule under a table's header.
  - Fenced code is not syntax-highlighted; Python highlights it with Pygments. A command or snippet in the approval panel is shown plain; a diff is coloured.
- **Expanding tool calls:** Ctrl+O opens a full-screen transcript view with every tool call expanded, rather than expanding the last block in place; Ctrl+Shift+O (or `a`) toggles them all in that view.
- **Model management:**
  - The typed `/model <id>` fallback asks the catalog of the active provider's config section. Python's asks the adapter's name, which is `openai` for every OpenAI-compatible gateway, so on OpenRouter it fetched OpenAI's catalog with OpenRouter's key; its bare `/model` picker already uses the section, as this does. Python should get the same fix.
  - The catalog picker has one box: Enter with nothing typed picks the highlighted model, and a highlight moved with the arrows is picked as it is. Python's filter box takes Enter as submitting the typed text (nothing typed cancels).
  - A long model list (more than nine choices) can be narrowed by typing, and its rows are not numbered; Python's model picker has no filter. PgUp, PgDn, Home, and End move through any picker.
  - The connection test, key entry, and save steps are panels in place of the input box, with numbered choices; the save panel switches scope with the arrows or Tab and scrolls the diff with the arrows. A catalog that arrives during a turn or while a panel is open waits until both are done.
  - The key entry shows one dot per character; Python's password field does the same.
  - The OS keyring is always reported as available, so key entry never says the key will only be used for the test.
- **MCP management:**
  - Editing a server in place keeps it running with its new settings after the save. Python's edit stops the server its test just started (it removes the "original" even when the name is unchanged), so the edited server reports `0 tools live` until the next launch. Python should get the same fix.
  - A test closed before the server finished connecting stops the server once it does, and a test closed after connecting stops it too; Python leaves such a server running, unmounted, for the rest of the session. A failed save also stops the server its test started.
  - The 30-second budget is the connect's own deadline, so a server that never answers reports Python's `did not finish connecting within 30s (connect_timeout_s)` message rather than the test modal's `Timed out after 30s — no response.`
  - JSON syntax errors and wrong field types are reported with `serde_json`'s wording, not Python's `json` and pydantic messages; the validator's own messages (a missing command or URL, both set, an unknown transport) are Python's.
  - The form is a panel: Tab, Shift+Tab, or the arrows move between fields, Enter in a one-line field continues (as in Python), Enter in the JSON box adds a line, and Left or Right switches the transport. Shift+Tab in the form does not cycle the approval mode.
  - Errors written to the transcript after a failed test have key-shaped tokens redacted; Python redacts only the copy in the test modal.

### Known loop differences from Python

- A deliberate fix: a sub-agent whose final reply stops at the output cap continues, as the main loop's answer does (up to 3 times, each a step of its budget, with a `continuation` event), and the pieces are joined; one that reaches the cap before any text (a thinking model can spend all of it reasoning) answers `[sub-agent '<role>' wrote no answer: ...]`, and an empty answer `[sub-agent '<role>' returned no text]`. Python returns the last reply as it is, so the parent gets a truncated or empty result it cannot tell from a real one.
- A deliberate fix: a sub-agent's requests carry the main loop's output cap (`max_output_tokens_for`, or the session's override). Python's sub-agents send none, so they get the provider's default (8192 on Anthropic, the server's own on an OpenAI-compatible endpoint), which neither `[limits] max_output_tokens` nor anything but the provider's `max_tokens` changes. On an OpenAI-compatible endpoint whose default is larger than 8192, a sub-agent's cap is now lower unless the cap is raised.
- A sub-agent's requests are fitted to a budget computed from the parent's system message and tools; Python reuses the budget of the parent's last request, which also counted that request's recall note.
- A route that cannot be built (an unknown provider, a missing key) fails the turn as a failed turn: it is saved in the `error` phase and traced with `turn_end`. Python raises before the turn is closed, so neither happens there.
- The window is looked up when the model is set (at start, on `/model`, and the first time a route is used), not before every request; a `[model.context_window]` change written by `/config` applies from the next `/model`.
- After a compaction mid-turn, the turn's start and a continuation in progress are re-indexed into the shorter history. Python keeps the old indices, which then point past where they should (a deliberate fix).
- The summary request can be cancelled; Python's blocks until it returns, and the turn stops at the next safe point.
- A cancel also wakes a response that has stopped streaming; Python checks only as each delta arrives.

### Known approval differences from Python

- The approval prompt is written to stderr; Python writes it to stdout, which would corrupt `run --json` output when a terminal is attached. The preview is shown as plain text, without Rich's syntax highlighting.
- Ctrl+C at the `run` prompt ends the process; Python treats it as declining.

### Known guardrail differences from Python

- Python's `\s` also matches the control characters U+001C to U+001F; the Rust `regex` crate's does not, so whitespace made of those characters can split a pattern differently. Otherwise the patterns use the same Unicode-aware `\b`, `\d`, `\s`, and case folding.

### Known sandbox differences from Python

- `python_exec` runs the `python` (Windows) or `python3` (elsewhere) found on PATH; Python runs its own interpreter (`sys.executable`).
- On Windows, a child's working directory is passed without the `\\?\` verbatim prefix, which `cmd.exe` cannot use.

### Known `web_fetch` differences from Python

- HTML is reduced with Python's regex fallback. Python uses BeautifulSoup instead when its optional `web` extra is installed, which extracts text differently.
- Connections go to the addresses that were vetted, so a DNS answer that changes between the check and the connection cannot redirect the fetch; Python resolves twice.
- The default User-Agent names this repository and version.
- Charsets are decoded with `encoding_rs` (WHATWG labels), except Latin-1, which is decoded as true ISO-8859-1 as in Python. Transport error text differs from httpx's.

### Known session differences from Python

- `state_json` is valid in both directions but not byte-identical: Rust omits empty lists and nulls that pydantic writes, and writes floats in Python's `json.dumps` style where pydantic uses shortest-decimal notation.

### Known skill differences from Python

- `SKILL.md` frontmatter is always read with Python's built-in parser. Python uses PyYAML instead when it happens to be installed (the `local` extra brings it in), which reads full YAML: nested mappings, anchors, and YAML's own quoting and folding rules. The usual shapes (`key: value`, lists, `>`/`|` descriptions) read the same either way. Quoted strings are unescaped as PyYAML reads them (`\"` and `''`), since a typical install has PyYAML; Python's parser alone keeps the backslashes. Where PyYAML rejects one quoted string it falls back to the built-in parser for the whole file, but here only that string is left as written. `license` and `metadata` are not kept, since nothing reads them.
- `/skills` puts each name on its own line (bold cyan), with the description indented under it and any `allowed-tools` after it, instead of Python's one `name - description` line per skill, which is hard to read once descriptions run past the terminal's width. Rich escapes markup in the names and descriptions; here they are plain text.

### Known memory differences from Python

- sentence-transformers vectors differ from Python's in the last two or three `float32` bits (about 3e-7), so a score within about 1e-6 of a recall threshold can fall on the other side of it.
- Only BERT sentence-transformers models (Transformer, Pooling, and Normalize modules; absolute positions; GELU) are supported. Python runs any model sentence-transformers can; others fall back here with `(ValueError)`, as an unloadable model does in Python. Plain transformers checkpoints without `modules.json` are not supported.
- The fallback note names the Rust failure's Python counterpart (`OSError` for a missing or unreachable model, `ValueError` for an unsupported one) and drops Python's `pip install "agent86[local]"` hint.
- Downloads are anonymous: `HF_TOKEN` is not sent, so gated or private models cannot be fetched. Files are written straight into the snapshot (as `huggingface_hub` does on Windows without symlinks) rather than as `blobs/` with links, which both libraries read; `.no_exist` markers are not written.
- With `hf_offline = false`, every start asks the hub for `main` (Python's `huggingface_hub` also checks each file's ETag).
- Search is always the brute-force scan. Python uses `sqlite-vec`'s `vec_distance_cosine` when the extension loads, which computes in `float32`, so its scores can differ from the scan's in the last digits.
- Scores follow Python 3.12's `sum`; under Python 3.11 (which the reference still supports) the last bit can differ.
- A `recall` `k` written as an integer literal beyond 64 bits arrives as a float and is refused as too large; pydantic accepts any integer and the clamp makes it 20.
- The `memory` commands print plain aligned text instead of Rich tables, and stored text is printed as is; Rich would interpret markup in it.
- A negative `memory search -k` is a usage error; Python slices from the end.

### Known MCP differences from Python

- The JSON-RPC client and transports are hand-written rather than the MCP Python SDK, so transport-level error wording (connection failures, HTTP errors) differs; the user-facing messages for start failures, timeouts, drops, reconnects, and unavailable servers follow Python's.
- A streamable HTTP server is never detected as dropped (requests simply fail), which matches the SDK in practice; stdio and SSE drops are detected when the process exits or the stream ends.
- A reconnect failure's cause is the first line of the start-failure message rather than the SDK's exception text. A drop's cause never carries Python's `after <last transport error>` suffix, because a malformed line is skipped without being recorded.
- `notifications/message` (server logging) is ignored; Python sends it to its logger, which `run` does not show. Live notes (progress, resource updates, list changes) are recorded for a future REPL, and `run` prints none, as in Python.
- The `initialize` request names the client `agent86` with the crate version; the Python SDK sends its own client info.
- On Windows a bare `command` is looked up on PATH with `PATHEXT`, so `npx` finds `npx.cmd`, as the Python SDK does.
- `mcp list` and `mcp tools` print plain aligned tables with the same columns as Python's Rich tables.

### Known flight-recorder differences from Python

- `trace show` prints a plain aligned table with the same columns as Python's Rich table.
- OpenTelemetry spans go over OTLP/HTTP with the JSON encoding (`<endpoint>/v1/traces`, `http://localhost:4318` by default), a deliberate choice that avoids a gRPC stack; Python's exporter speaks gRPC (`localhost:4317`) and falls back to HTTP with protobuf. The OpenTelemetry Collector, Jaeger, and Tempo accept both by default, but an endpoint that only serves gRPC receives nothing. `otel_endpoint` is a base URL that gets `/v1/traces` appended (unless it already ends with it); Python hands it to the gRPC exporter as the host.
- The resource carries `service.name`, `service.version` (the crate's), and `telemetry.sdk.language = "rust"`, not the Python SDK's `telemetry.sdk.*` values. `exception` events carry `exception.type` and `exception.message`, without a stack trace.
- A deliberate fix: read-only tool calls run in parallel are children of the turn span. Python runs them on a thread pool that does not carry the span context, so each becomes the root of a trace of its own.
- An export that fails is kept as the tracer's note and not printed; Python's SDK logs a warning to stderr. `otel = true` always exports here; Python needs its `otel` extra installed and is otherwise a no-op.
- Events for subsystems that are not ported (`cancelled` turns) are never written.
- `limit_raised` (a limit raised from the interface's offer) has no Python counterpart.

### Known tool differences from Python

- I/O failures other than the handled cases report Rust's error text after Python's exception name (for example `PermissionError: Access is denied. (os error 5)` rather than Python's `[Errno 13] Permission denied: '...'`).
- Invalid UTF-8 is replaced per maximal invalid sequence (Rust's `from_utf8_lossy`) rather than per byte, so a run of bad bytes can yield fewer U+FFFD characters than Python.

### Known provider differences from Python

- Anthropic: the adapter speaks the HTTP API rather than going through the SDK. A stream cut short reports `ReadError:` with the transport error's text from `reqwest` (Python's loop names httpx's exception, usually `RemoteProtocolError`), and a malformed event names `serde_json`'s parse error after `JSONDecodeError:`. Tool input that is not valid JSON when its block ends is carried raw under `__invalid_json__`, as the other adapters do, where the SDK would keep whatever its partial-JSON parser salvaged. A `Retry-After` HTTP date falls back to backoff. The SDK's own request headers (`user-agent`, `x-stainless-*`) are not sent.

- Ollama: a stream cut short reports the transport error's text from `reqwest` rather than httpx's `<ExceptionName>: <message>`, and a malformed NDJSON line names `serde_json`'s parse error rather than Python's `JSONDecodeError` text; the rest of each message is Python's. A thinking model's `thinking` field is ignored, as in Python, so a model that spends its whole turn thinking answers with nothing.

- `Retry-After` HTTP-date values are not parsed; they fall back to exponential backoff. Delta-seconds values are honoured.
- Python retries only `ConnectError`, `ConnectTimeout`, and `RemoteProtocolError`. `reqwest` does not expose the same classes, so Rust retries any transport failure other than a read timeout, still only before text has been emitted.
- Like Python, an SSE `data:` chunk carrying an `{"error": ...}` object (sent by OpenRouter for mid-stream failures) is silently ignored. Both implementations should probably surface it; change them together.

## 4. First milestone (complete): one real OpenAI-compatible model turn

The next milestone is a minimal text-only, one-shot CLI using an OpenAI-compatible provider, with OpenRouter as the first live target.

OpenRouter comes before Ollama because it:

- Requires no local model runtime or model download
- Exercises a remote HTTPS workflow available to many users
- Uses an OpenAI-compatible implementation that can later support Groq, Together, vLLM, LM Studio, and custom endpoints
- Allows deterministic development with recorded fixtures and mock servers, without credentials

### Planned scope

1. **Async provider abstraction**
   - Streaming and non-streaming completion interfaces
   - Provider-neutral errors
   - Connection and response timeout handling
   - No retry after streamed output has been emitted

2. **OpenAI-compatible provider**
   - Chat-completions request construction
   - SSE stream decoding
   - Text delta and final usage handling
   - Clean handling of malformed, empty, and truncated streams
   - OpenRouter `base_url` and `api_key_env` configuration conventions

3. **Minimal one-shot CLI**
   - `--model <provider:model>`
   - `run <prompt>`
   - `run --json <prompt>`
   - Text-only turns; no tools or ReAct loop in this milestone
   - Preserve the established JSON field names from the beginning: `session_id`, `output`, `steps`, `usage`, and `turn`
   - An ephemeral session identifier is acceptable until persistence is implemented

4. **Deterministic tests**
   - Recorded SSE fixtures
   - A local mock HTTP server for integration tests
   - Success, empty response, malformed stream, truncated stream, timeout, and provider-error cases
   - No credentials required for normal tests

5. **Optional live smoke test**
   - Run against OpenRouter only when the user supplies credentials through a secure environment or local keyring
   - Use a low-cost model selected by the user
   - Keep live tests opt-in and outside normal CI

### Acceptance criteria

All criteria were met on 2026-10-26.

- Formatting, tests, and Clippy pass
- The CLI executes against the mock provider without external credentials
- Streamed and non-streamed output produce the same final text
- Provider failures become clean, structured errors rather than panics or partial success output
- `run --json` emits one JSON object on stdout and errors on stderr
- API keys never appear in logs, errors, tests, commits, or documentation
- A live OpenRouter smoke test succeeds when suitable credentials and network access are available

## 5. Delivery plan after the first model turn

| Stage | Planned work | Main compatibility concern |
|---|---|---|
| 1 | OpenAI-compatible provider and one-shot CLI | Streaming, errors, JSON output |
| 2 | Ollama provider behind the same abstraction | NDJSON and local-model behavior |
| 3 | Read-only workspace tools | Path jails, schemas, ordered results |
| 4 | ReAct loop and circuit breakers | State transitions, limits, cancellation |
| 5 | Approvals, guardrails, and side-effecting tools | Security behavior and write ordering |
| 6 | Subprocess and Docker sandboxing | Environment scrubbing and process-tree cleanup |
| 7 | Sessions, memory, and observability | SQLite, retention, and trace compatibility |
| 8 | MCP client | Transports, notifications, reconnects, and lifecycle races |
| 9 | Interactive terminal interface | Python's interactive behavior and cancellation, presented inline in Claude Code's style ([ADR 0001](adr/0001-inline-terminal-interface.md)) |
| 10 | Local embedding options | Model runtime and offline behavior |

Each stage should produce a usable vertical slice and be compared with the Python implementation before the project moves on.

Stage 10 was delivered in two parts:

- [x] Part 1: episodic and semantic memory, the memory tools and commands, and retention, with the hashing embedder
- [x] Part 2: the `sentence-transformers` embedder (`all-MiniLM-L6-v2`) on candle, producing vectors that match Python's so memory is shared both ways

Stage 9 is delivered in parts, one PR each. The details and acceptance tests for parts 2 to 6 are in [docs/INTERFACE.md](INTERFACE.md), section 10.

- [x] Part 1: the plain REPL, commands, mentions, history, and turn cancellation
- [x] Part 2: the inline base: input box, streaming transcript, tool blocks, spinner, status line, approval panel, cancel and quit keys, and the plain-loop fallback
- [x] Part 3: the `/` command menu, `@` path completion, and the mode, model, and session pickers
- [x] Part 4: Markdown replies, coloured diffs, and the Ctrl+O transcript view
- [x] Part 5: model catalogs, the provider manager, key entry, the connection test, and comment-preserving config write-back
- [x] Part 6: MCP server management (add, edit, remove, enable or disable, test) with live mounting

## 6. Compatibility strategy

The Python implementation is the behavioral oracle. Compatibility should be tested through observable behavior, not by copying implementation details.

Contracts to preserve include:

- CLI arguments, stdout/stderr separation, and exit codes
- `run --json` field names and failure behavior
- TOML configuration resolution and write-back semantics
- Provider streaming, stop-reason normalization, usage, and pricing behavior
- Tool schemas, validation failures, approvals, and result ordering
- Session persistence and resumability
- Flight-recorder and export formats
- Graceful degradation when optional capabilities are unavailable

Recorded provider responses were introduced with the first provider; parity is checked by fixtures generated from Python's sources and CPython rather than a differential harness (see the decision under Next actions). Existing Python tests should guide expected behavior wherever practical.

The Rust runtime should be designed around explicit async ownership and cancellation rather than reproducing the Python mixture of threads and event loops literally.

## 7. Credential and security policy

- API keys must never be pasted into chat, committed, or written to configuration files
- Live providers should resolve secrets from environment variables or an OS keyring
- Tests must use fixtures, mock servers, or explicitly scoped test credentials
- Secret-shaped values should be redacted from logs, traces, and error paths
- Live tests must be opt-in and should use the least expensive suitable model
- Rust memory safety does not replace SSRF defenses, sandboxing, path validation, or secret redaction

A feature request has been drafted for Kimi to support secure, native secret input for sandbox tasks. It has not been submitted through the user's account. This does not block development or deterministic testing.

## 8. Working principles

- Keep the Python implementation as the reference until Rust reaches proven parity
- Prefer small, tested vertical slices over large rewrites
- Do not claim performance improvements without benchmarks
- Do not broaden features during the compatibility port
- Keep credentials and live services out of the default test path
- Treat security behavior and failure paths as compatibility requirements
- Keep the repository honest about what is and is not implemented

## 9. Next actions

- [x] Implement the async provider interface and OpenAI-compatible SSE client
- [x] Add recorded fixtures and mock-provider integration tests
- [x] Add TOML provider config, env-var key resolution, and the `provider:model` factory
- [x] Implement text-only `run` and `run --json`
- [x] Validate locally without credentials
- [x] Perform an optional live OpenRouter smoke test (2026-10-26, `openrouter:openai/gpt-4o-mini`)
- [x] Upload the CI workflow
- [x] The remaining CLI commands (`models`, `config`, `skills`, `--sandbox`)
- [x] Triage routing (`orchestration/router.py`): `[model] router = "triage"`, the cheap and frontier routes, the `route` trace event, compaction on the cheap model, and pinning on `/model`
- [x] Live OpenTelemetry span export (`[observability] otel`)

Decided (2026-10-28): no differential test harness that runs the Python implementation. Parity is pinned instead by per-feature fixtures produced from Python's own sources or CPython (`ast` extractions, CPython `re`, `difflib`, `ipaddress`, `json`, and `sum` outputs, sentence-transformers vectors, mirror scripts of the pydantic models and trace code) and by recorded provider streams. Running the reference implementation would mean executing third-party code on the development machine, and the fixtures already cover the contract. Revisit if a behavior turns out to have drifted in a way the fixtures did not catch.

Parked (2026-10-28): output caps that depend on the model, such as a larger default for models known to think. `/limits` and the interface's offer to raise a limit cover the case without a table of thinking models to keep current, and it would change defaults Python users rely on. Revisit if the offer proves too frequent in practice.
