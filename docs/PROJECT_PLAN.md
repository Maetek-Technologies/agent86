# agent86-rs Project Plan

**Last updated:** 2026-10-26  
**Current stage:** First milestone complete; pricing, the system prompt, and keyring keys ported; `agent86 keys` added; read-only workspace tools (stage 3) and the ReAct loop (stage 4) ported; approvals, file-writing tools, guardrails (stage 5), sandboxed command and Python execution (stage 6), `web_fetch`, sessions, and the flight recorder (stage 7) done; the MCP client (stage 8) done; the plain interactive REPL with turn cancellation (stage 9, part 1) done; the interactive interface (stage 9, parts 2 to 6) planned as an inline, Claude Code-style interface ([ADR 0001](adr/0001-inline-terminal-interface.md), [design](INTERFACE.md)), with part 2 (the inline base) next, and episodic and semantic memory deferred to stage 10

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
- Added secret redaction ported from `secrets.py`, applied to error bodies and transport details
- Added recorded SSE fixtures and a scripted local mock HTTP server covering success, tool calls, empty, unterminated, malformed, truncated, stalled, unreachable, client-error, retry, and retry-exhaustion cases

Local validation completed successfully:

- `cargo test --all-targets`: **243 passed on Windows** (64 unit, 10 compatibility, 18 provider integration, 32 CLI end-to-end, 11 REPL end-to-end, 5 sessions, 8 trace, 17 MCP, 13 tools, 11 write tools and approvals, 11 exec tools, 11 web fetch, 2 difflib, 2 guardrails, 24 agent loop, 2 system prompt, 2 API key; some exec tests are platform-specific), plus 2 opt-in live keyring tests
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

- Config sections other than `[model]`, `[limits]`, `[sandbox]`, `[guardrails]`, `[tools]`, `[memory] enabled`/`path`, `[observability]`, `[mcp]`/`[mcp_servers]`, `[ui]`, `[pricing]`, and `[providers.*]` (others are accepted and ignored, not validated)
- Anthropic, Ollama, and llama.cpp providers
- Triage routing (`[model] router = "triage"` is read but every turn uses the default model)
- Context-window fitting and compaction of long conversations
- Memory, skills, and delegation tools, and skills themselves
- Episodic and semantic memory, retention pruning, and OpenTelemetry span export (`[observability] otel`)
- The interactive interface that replaces Python's full-screen Textual app: the status line, key bindings, the command menu, mention completion, Markdown replies, and the model, provider, key, MCP, session, and approval screens. It is planned in [docs/INTERFACE.md](INTERFACE.md).
- Local embedding backend

### Known `run` differences from Python

- The system prompt is the identity, the environment line, and the MCP section when servers are live. Python also adds installed skills and an episodic recall note, which are not ported.
- The environment line reports the Rust crate version (`agent86 v0.1.0`), not Python's (`v1.0.0`).
- `--sandbox` is not accepted (`AGENT86_SANDBOX` and `[sandbox] mode` are).
- Usage is priced under the full `provider:model` ref rather than the bare model id. Python prices with the bare id, so a `[pricing.models]` override keyed by the ref (for example `"openrouter:openai/gpt-4o-mini"`) marks the model as priced but never applies the rate, and the summary shows `$0.0000`. The ref is tried first and then the bare id, so every other case matches Python. Python should get the same fix.
- `agent86 keys` has no Python CLI equivalent; Python stores keys from its interactive `/config model` screen. Re-saving an account on Windows does not leave Python's stale `<account>@agent86` copy of the old key behind.
- The missing-key error names the keyring entry to create (service `agent86`, account `<provider>`) instead of pointing at `/config model`.
- Reading keys that Python stored is verified on Windows against the `keyring` package's source, and live on 2026-10-26: a key saved with Python's `keyring set agent86 openrouter` was read by Rust and accepted by OpenRouter. On macOS and Linux the attribute layouts match by inspection but have not been tested against entries Python wrote. `tests/os_keyring.rs` is an opt-in check for any platform.
- The model-error hint suggests `--model` and an API key instead of the `agent86 models` command, which is not ported. The malformed-config hint does not mention `agent86 config path` for the same reason.
- Rich wraps stderr at 80 columns when it is not a terminal; Rust does not wrap. stdout, including `--json` output (2-space indent, non-ASCII unescaped), is unaffected.

### Known REPL differences from Python

- The interactive interface will render inline, in Claude Code's style, rather than as Python's full-screen Textual app ([ADR 0001](adr/0001-inline-terminal-interface.md)). Its commands, messages, approvals, files, and cancellation stay Python's. The differences it introduces are planned in [docs/INTERFACE.md](INTERFACE.md) and will be listed here as each slice lands.
- The plain loop is the only surface so far. Python opens its full-screen Textual app when `[ui] tui` is on and stdin and stdout are terminals, and falls back to the plain loop otherwise; `--plain`, `AGENT86_PLAIN`, and `[ui] tui` are accepted but change nothing yet, and no "TUI unavailable" note is printed.
- Ctrl+C during a turn cancels it at the next safe point (`[cancelled]`, saved, traced as cancelled), which is what Python's full-screen app does with its cancel key. Python's plain loop instead raises `KeyboardInterrupt`, prints `interrupted`, and abandons the turn without closing or saving it. A running tool is never interrupted: the cancel takes effect when it finishes.
- Ctrl+C at an approval prompt does not decline; the prompt waits for an answer and the turn stops before the next call. Python declines.
- A cancel that lands after the model asked for tools but before any ran answers each call with `Not executed: cancelled.`; Python leaves those calls unanswered, which providers reject on the next request.
- `/config` prints the merged TOML layers (built-in defaults for the modelled sections, the config files, and overrides) as JSON, not Python's full pydantic dump with every default.
- `/config model` and `/config mcp` say the full-screen interface is not ported and point at `/model`, `agent86 keys`, and `agent86 mcp list`; Python's plain loop points at its full-screen app.
- `/skills` always reports `no skills discovered`, and `/memory`'s episode and fact counts stay at whatever Python wrote, because neither is ported.
- `/models` always reports the OS keyring as available; Python checks for a usable backend. Its tables, `/sessions`, and the help list are plain aligned text (Rich tables in Python, with the same columns), and nothing is wrapped to the terminal width.
- `/model` switches the provider for later turns; there is no router to pin, since triage routing is not ported.
- Answers are printed as plain text, as in Python's plain loop; the Markdown rendering behind `[ui] markdown` belongs to the full-screen app.

### Known loop differences from Python

- Python also offers memory, skill, and delegation tools when those are enabled, so the tool list in each request differs.
- The conversation is not fitted to the model's context window and never compacted; a very long tool loop could exceed the window and fail with a provider error.
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

- Python records an episode per turn and adds an episodic recall note to the system prompt; neither is ported, because they need embeddings. The `episodes` and `memories` tables are created but not written.
- `state_json` is valid in both directions but not byte-identical: Rust omits empty lists and nulls that pydantic writes, and writes floats in Python's `json.dumps` style where pydantic uses shortest-decimal notation.
- A long resumed session is sent in full; Python fits it to the model's context window and compacts it (not ported).

### Known MCP differences from Python

- The JSON-RPC client and transports are hand-written rather than the MCP Python SDK, so transport-level error wording (connection failures, HTTP errors) differs; the user-facing messages for start failures, timeouts, drops, reconnects, and unavailable servers follow Python's.
- A server that stays disconnected after its reconnect attempts reports that its tools are unavailable until agent86 is restarted; Python points at `/config mcp`, which is not ported.
- A streamable HTTP server is never detected as dropped (requests simply fail), which matches the SDK in practice; stdio and SSE drops are detected when the process exits or the stream ends.
- A reconnect failure's cause is the first line of the start-failure message rather than the SDK's exception text. A drop's cause never carries Python's `after <last transport error>` suffix, because a malformed line is skipped without being recorded.
- `notifications/message` (server logging) is ignored; Python sends it to its logger, which `run` does not show. Live notes (progress, resource updates, list changes) are recorded for a future REPL, and `run` prints none, as in Python.
- The `initialize` request names the client `agent86` with the crate version; the Python SDK sends its own client info.
- On Windows a bare `command` is looked up on PATH with `PATHEXT`, so `npx` finds `npx.cmd`, as the Python SDK does.
- `mcp list` and `mcp tools` print plain aligned tables with the same columns as Python's Rich tables, and the no-servers hint does not mention `/config mcp`.
- Adding, removing, or restarting servers mid-session (`/config mcp`, `add_mcp_server`) belongs to the interactive interface and is not ported.

### Known flight-recorder differences from Python

- `trace show` prints a plain aligned table with the same columns as Python's Rich table.
- Live OpenTelemetry export (`[observability] otel`) is not ported; `trace export -f otlp-json` covers the offline case.
- Events for subsystems that are not ported (`compaction`, `route`, sub-agent `spawn`, `retention_prune`, and `cancelled` turns) are never written.

### Known tool differences from Python

- I/O failures other than the handled cases report Rust's error text after Python's exception name (for example `PermissionError: Access is denied. (os error 5)` rather than Python's `[Errno 13] Permission denied: '...'`).
- Invalid UTF-8 is replaced per maximal invalid sequence (Rust's `from_utf8_lossy`) rather than per byte, so a run of bad bytes can yield fewer U+FFFD characters than Python.
- The jail has no extra allowed roots yet; Python adds skill directories, which are not ported.

### Known provider differences from Python

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

Stage 9 is delivered in parts, one PR each. The details and acceptance tests for parts 2 to 6 are in [docs/INTERFACE.md](INTERFACE.md), section 10.

- [x] Part 1: the plain REPL, commands, mentions, history, and turn cancellation
- [ ] Part 2: the inline base: input box, streaming transcript, tool blocks, spinner, status line, approval panel, cancel and quit keys, and the plain-loop fallback
- [ ] Part 3: the `/` command menu, `@` path completion, and the mode, model, and session pickers
- [ ] Part 4: Markdown replies, coloured diffs, and the Ctrl+O transcript view
- [ ] Part 5: model catalogs, the provider manager, key entry, the connection test, and comment-preserving config write-back
- [ ] Part 6: MCP server management (add, edit, remove, enable or disable, test) with live mounting

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

Recorded provider responses and differential tests should be introduced with the first provider. Existing Python tests should guide expected behavior wherever practical.

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
