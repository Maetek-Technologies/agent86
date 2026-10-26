# agent86-rs Project Plan

**Last updated:** 2026-10-26  
**Current stage:** OpenAI-compatible provider complete; one-shot `run` CLI next

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
- Added secret redaction ported from `secrets.py`, applied to error bodies and transport details
- Added recorded SSE fixtures and a scripted local mock HTTP server covering success, tool calls, empty, unterminated, malformed, truncated, stalled, unreachable, client-error, retry, and retry-exhaustion cases

Local validation completed successfully:

- `cargo test --all-targets`: **38 passed** (12 unit, 10 compatibility, 16 provider integration)
- `cargo fmt --all -- --check`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- CLI `--help` and `--version` execution: passed

### Not implemented

The project is **not yet a usable agent**. It currently has no:

- Live model calls from the CLI (the provider exists but nothing invokes it yet)
- Provider configuration loading, API-key resolution from the environment or keyring, or the provider factory
- Pricing (`Usage.cost_usd` is always 0.0)
- An Ollama (NDJSON) provider
- One-shot `run` implementation
- Tool execution or ReAct orchestration
- Guardrails, approvals, or sandbox executors
- Session persistence, memory, or tracing
- MCP support
- Interactive terminal interface
- Local embedding backend

### Known provider differences from Python

- `Retry-After` HTTP-date values are not parsed; they fall back to exponential backoff. Delta-seconds values are honoured.
- Python retries only `ConnectError`, `ConnectTimeout`, and `RemoteProtocolError`. `reqwest` does not expose the same classes, so Rust retries any transport failure other than a read timeout, still only before text has been emitted.
- Like Python, an SSE `data:` chunk carrying an `{"error": ...}` object (sent by OpenRouter for mid-stream failures) is silently ignored. Both implementations should probably surface it; change them together.

## 4. Immediate milestone: one real OpenAI-compatible model turn

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
- [ ] Add TOML provider config, env-var key resolution, and the `provider:model` factory
- [ ] Implement text-only `run` and `run --json`
- [ ] Validate locally without credentials
- [ ] Perform an optional live OpenRouter smoke test
- [x] Upload the CI workflow
