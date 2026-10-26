# agent86-rs Project Plan

**Last updated:** 2026-10-26  
**Current stage:** First milestone complete; pricing, the system prompt, and keyring keys ported; `agent86 keys` added; read-only workspace tools (stage 3) and the ReAct loop (stage 4) ported; approvals, file-writing tools, guardrails (stage 5), sandboxed command and Python execution (stage 6), `web_fetch`, and sessions (stage 7, part 1) done; the flight recorder (stage 7, part 2) next

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
- Added secret redaction ported from `secrets.py`, applied to error bodies and transport details
- Added recorded SSE fixtures and a scripted local mock HTTP server covering success, tool calls, empty, unterminated, malformed, truncated, stalled, unreachable, client-error, retry, and retry-exhaustion cases

Local validation completed successfully:

- `cargo test --all-targets`: **181 passed on Windows** (46 unit, 10 compatibility, 18 provider integration, 29 CLI end-to-end, 5 sessions, 13 tools, 11 write tools and approvals, 11 exec tools, 11 web fetch, 2 difflib, 2 guardrails, 19 agent loop, 2 system prompt, 2 API key; some exec tests are platform-specific), plus 2 opt-in live keyring tests
- `cargo fmt --all -- --check`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- CLI `run` and `run --json` against the mock provider: passed
- Live session check (2026-10-26, `openrouter:openai/gpt-4o-mini`): a turn that read a file was saved; its `state_json` validated against pydantic mirrors of Python's models; and `run -s <id>` resumed it, with the model recalling the file's last item without tools
- Live `web_fetch` check (2026-10-26, `openrouter:openai/gpt-4o-mini`): the model fetched https://example.com and reported its heading, and a fetch of `http://169.254.169.254/latest/meta-data/` was refused before any connection
- Live sandbox check (2026-10-26, `openrouter:openai/gpt-4o-mini`, `run --yes`): the model ran `python_exec` (sum of squares, 385) and `run_command` (`ls`) in one step and reported both correctly
- Live file-writing check (2026-10-26, `openrouter:openai/gpt-4o-mini`, `run --yes` in a throwaway directory): the model called `edit_file` and `write_file` in one step, and both files on disk were correct
- Live agent-loop check (2026-10-26, `openrouter:openai/gpt-4o-mini`): asked to inspect this repository, the model called `list_dir`, then `read_file README.md` and `list_dir tests` in one parallel step, and answered correctly (3 steps, 3 tools, about 2.7k input tokens)
- Live keyring check (2026-10-26): with no environment variable set, `run` used the OpenRouter key from Windows Credential Manager and sent the system prompt (337 input tokens)
- Live OpenRouter smoke test (2026-10-26, `openrouter:openai/gpt-4o-mini`): `run` streamed the answer with the summary line on stderr, and `run --json` produced the contract fields with real token usage (12 in / 7 out). The key was read from the environment and appeared in no output

### Not implemented

The project is **not yet a usable agent**. It currently has no:

- Config sections other than `[model]`, `[limits]`, `[sandbox] mode`, `[pricing]`, and `[providers.*]` (others are accepted and ignored, not validated)
- Anthropic, Ollama, and llama.cpp providers
- Side-effecting tools (`write_file`, `edit_file`, `run_command`, `python_exec`), `web_fetch`, memory, skills, and delegation tools
- Guardrails, approvals, or sandbox executors
- Episodic and semantic memory, or the flight recorder and tracing
- MCP support
- Interactive terminal interface
- Local embedding backend

### Known `run` differences from Python

- No tools are offered. The system prompt is the identity plus the environment line, which is what Python sends when no skills are installed, no MCP servers are configured, and episodic memory has nothing to recall. Those three optional sections are not ported.
- The environment line reports the Rust crate version (`agent86 v0.1.0`), not Python's (`v1.0.0`).
- Answers cut off by `max_tokens` are not continued automatically, and the egress guardrail is not applied.
- The session id is ephemeral and is not persisted; `--session`/`-s`, `--yes`, `--sandbox`, `--approval`, `--resume`, and `--plain` are not accepted yet.
- With no subcommand, Python opens the REPL; Rust prints a not-implemented message and exits 2.
- Usage is priced under the full `provider:model` ref rather than the bare model id. Python prices with the bare id, so a `[pricing.models]` override keyed by the ref (for example `"openrouter:openai/gpt-4o-mini"`) marks the model as priced but never applies the rate, and the summary shows `$0.0000`. The ref is tried first and then the bare id, so every other case matches Python. Python should get the same fix.
- `agent86 keys` has no Python CLI equivalent; Python stores keys from its interactive `/config model` screen. Re-saving an account on Windows does not leave Python's stale `<account>@agent86` copy of the old key behind.
- The missing-key error names the keyring entry to create (service `agent86`, account `<provider>`) instead of pointing at `/config model`.
- Reading keys that Python stored is verified on Windows against the `keyring` package's source, and live on 2026-10-26: a key saved with Python's `keyring set agent86 openrouter` was read by Rust and accepted by OpenRouter. On macOS and Linux the attribute layouts match by inspection but have not been tested against entries Python wrote. `tests/os_keyring.rs` is an opt-in check for any platform.
- The model-error hint suggests `--model` and an API key instead of the `agent86 models` command, which is not ported. The malformed-config hint does not mention `agent86 config path` for the same reason.
- Rich wraps stderr at 80 columns when it is not a terminal; Rust does not wrap. stdout, including `--json` output (2-space indent, non-ASCII unescaped), is unaffected.

### Known loop differences from Python

- Python also offers memory, skill, and delegation tools when those are enabled, so the tool list in each request differs.
- The conversation is not fitted to the model's context window and never compacted; a very long tool loop could exceed the window and fail with a provider error.
- Tool-call arguments are not egress-scanned; Python only records those findings in its flight recorder, which is not ported.
- There is no cancellation (Ctrl-C ends the process) and no flight recorder.

### Known approval differences from Python

- The approval prompt is written to stderr; Python writes it to stdout, which would corrupt `run --json` output when a terminal is attached. The preview is shown as plain text, without Rich's syntax highlighting.
- Ctrl+C at the prompt ends the process; Python treats it as declining.

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
| 9 | Interactive terminal interface | Textual-equivalent UX and cancellation |
| 10 | Local embedding options | Model runtime and offline behavior |

Each stage should produce a usable vertical slice and be compared with the Python implementation before the project moves on.

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
