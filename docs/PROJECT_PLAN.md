# agent86-rs Project Plan

**Last updated:** 2026-10-26  
**Current stage:** First milestone complete; pricing, the system prompt, and keyring keys ported

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
- Added secret redaction ported from `secrets.py`, applied to error bodies and transport details
- Added recorded SSE fixtures and a scripted local mock HTTP server covering success, tool calls, empty, unterminated, malformed, truncated, stalled, unreachable, client-error, retry, and retry-exhaustion cases

Local validation completed successfully:

- `cargo test --all-targets`: **78 passed** (32 unit, 10 compatibility, 18 provider integration, 14 CLI end-to-end, 2 system prompt, 2 API key), plus 1 opt-in live keyring test
- `cargo fmt --all -- --check`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- CLI `run` and `run --json` against the mock provider: passed
- Live keyring check (2026-10-26): with no environment variable set, `run` used the OpenRouter key from Windows Credential Manager and sent the system prompt (337 input tokens)
- Live OpenRouter smoke test (2026-10-26, `openrouter:openai/gpt-4o-mini`): `run` streamed the answer with the summary line on stderr, and `run --json` produced the contract fields with real token usage (12 in / 7 out). The key was read from the environment and appeared in no output

### Not implemented

The project is **not yet a usable agent**. It currently has no:

- Storing or deleting keyring entries (Python does this from its interactive `/config model` screen, which is not ported)
- Config sections other than `[model]`, `[limits]`, `[sandbox] mode`, `[pricing]`, and `[providers.*]` (others are accepted and ignored, not validated)
- Anthropic, Ollama, and llama.cpp providers
- Tool execution or ReAct orchestration
- Guardrails, approvals, or sandbox executors
- Session persistence, memory, or tracing
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
- The missing-key error names the keyring entry to create (service `agent86`, account `<provider>`) instead of pointing at `/config model`.
- Reading keys that Python stored is verified on Windows against the `keyring` package's source, and live on 2026-10-26: a key saved with Python's `keyring set agent86 openrouter` was read by Rust and accepted by OpenRouter. On macOS and Linux the attribute layouts match by inspection but have not been tested against entries Python wrote. `tests/os_keyring.rs` is an opt-in check for any platform.
- The model-error hint suggests `--model` and an API key instead of the `agent86 models` command, which is not ported. The malformed-config hint does not mention `agent86 config path` for the same reason.
- Rich wraps stderr at 80 columns when it is not a terminal; Rust does not wrap. stdout, including `--json` output (2-space indent, non-ASCII unescaped), is unaffected.

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
