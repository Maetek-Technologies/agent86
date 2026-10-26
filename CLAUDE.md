# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A compatibility-first Rust port of [agent86](https://github.com/Maetek-Technologies/agent-86), a Python agentic CLI harness. **The Python implementation is the behavioral oracle**: the Rust code must reproduce its observable behavior (CLI output streams, exit codes, `run --json` fields, TOML config semantics, provider streaming/tool-call behavior, approvals, session/trace formats, error degradation), not its internal structure.

The CLI can run one text-only turn against any OpenAI-compatible provider (`agent86 --model provider:model run [--json] <goal>`), with Python's system prompt, but there are no tools or agent loop yet. `docs/PROJECT_PLAN.md` is the source of truth for current status, the next steps, the staged roadmap, known differences from Python, and the credential policy. Read it before starting feature work, and update it (plus the README status/roadmap) when a stage lands. Keep both honest about what is and is not implemented.

## Commands

```bash
cargo test --all-targets                     # all tests
cargo test --test compatibility              # just the compatibility suite
cargo test --test openai_provider            # provider tests against the local mock server
cargo test --test cli_run                    # end-to-end tests that run the agent86 binary
cargo test <test_name_substring>             # single test, e.g. cargo test model_refs_split
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings    # warnings are errors
cargo run -- --help
```

All three checks (test, fmt, clippy `-D warnings`) must pass. `unsafe_code` is forbidden at the crate level. MSRV is Rust 1.88 (`rust-version` in Cargo.toml, set by `tokio`/`reqwest` and their URL dependencies); avoid newer language/std features, and check a new dependency's `rust-version` before adding it. `.cargo/config.toml` makes the resolver prefer MSRV-compatible versions, but platform-specific dependencies (for example the Linux keyring stack) only build in CI, so watch the Linux and macOS MSRV jobs. Raising the MSRV is a deliberate change: update Cargo.toml and the CI matrix together.

CI (`.github/workflows/ci.yml`) runs tests on Linux, macOS, and Windows against both stable and the MSRV, with fmt and clippy on stable. Windows builds need the Visual Studio Build Tools C++ workload for `link.exe`.

## Architecture

- `src/types.rs`: the provider-neutral vocabulary everything else will build on: `Role`, `AgentPhase`, `ApprovalMode`, `StopReason`, `ModelRef`, `ToolSpec`/`ToolCall`/`ToolResult`, `Message`, `CompletionRequest`/`Completion`/`CompletionDelta`, `Step`, `Usage` (implements `Add` for accumulation).
- `src/provider/`: the model layer, ported from Python's `agent86/cognitive/`.
  - `mod.rs`: the object-safe `Provider` trait, `ProviderError`, and `provider_for_ref`, the factory. Any provider section with a `base_url` becomes the OpenAI-compatible adapter; `anthropic`, `ollama`, and `llamacpp` return `NotPorted` because Python gives them dedicated adapters. Providers implement only `stream()`, which returns a boxed stream of `CompletionDelta`s whose last item has `done = true` plus the full `Completion`. `complete()` is derived by collecting that stream, so streamed and non-streamed text always agree. `ProviderError` messages mirror the Python wording and are stored already redacted.
  - `openai.rs`: the OpenAI-compatible SSE provider (OpenAI, OpenRouter, Groq, local servers). The stream is a hand-written state machine (`StreamState`) driven by `futures_util::stream::unfold`; dropping the stream drops the HTTP response, which is how cancellation works. Per-attempt accumulators live in `TurnState` and reset on every retry.
  - `retry.rs`: retryable statuses, `Retry-After`, and jittered backoff. A retry is never allowed once a text delta has been emitted, because the caller already saw that text.
  - `capabilities.rs`: models that reject sampling parameters.
- `src/secrets.rs`: `redact()`, ported from Python's `secrets.py`. Run every response body or transport error through it before it reaches a `ProviderError`. Also `resolve_api_key()`: the `api_key_env` variable, then a `SecretStore` (`OsKeyring` in production; tests pass a fake or `NoSecretStore` through `provider_for_ref_with_store`, and must never touch the real keyring). On Windows `resolve_windows_credential` reproduces the Python `keyring` layout (`agent86`, then `<account>@agent86`); keep it that way so keys Python stored stay readable.
- `src/pricing.rs`: `PriceTable`, ported from `pricing.py` (built-in rates, local providers at zero, dated-snapshot matching, cache rates, `[pricing.models]` overrides). Python keeps overrides in a module global; here the table is passed to the provider with `with_pricing`, and usage is priced under the full `provider:model` ref (a deliberate fix, see the plan). Unknown models cost `0.0` but must display as `cost n/a`, so ask `is_priced` before showing a dollar figure.
- `src/pyjson.rs`: `python_float`, a `serialize_with` that writes floats exactly as Python's `json.dumps` does (`1.44e-05`). Use it on every `f64` field that reaches `run --json` or another Python-compatible JSON output.
- `src/prompt.rs`: the system prompt. `BASE_IDENTITY` must stay byte-identical to Python's `_BASE_IDENTITY`; `tests/system_prompt.rs` compares it with `tests/fixtures/prompt/base_identity.txt`, which was extracted from the Python source with `ast` (parsed, never executed). Re-extract it the same way when the reference changes.
- `src/keys.rs`: `agent86 keys set|delete`. All checks (known provider with `api_key_env`, non-empty single paste) run before the keyring is written, so CLI tests cover only refusals; never add a test that stores a real key. Hidden input uses `rpassword`, except under MSYS (`MSYSTEM` set, i.e. Git Bash), where console prompts cannot read input and `stty -echo` is used instead. On Windows the store leaves `UserName` empty for credentials created under an explicit target, so `CredentialVault::write` must set it explicitly; `tests/os_keyring.rs` has an opt-in live round trip that catches this.
- `src/config.rs`: layered TOML config: built-in defaults (a TOML string), the user file, the project file, `AGENT86_MODEL`, then `--model`, deep-merged as `toml::Table`s before deserializing. Only `[model]`, `[limits]`, `[sandbox] mode`, `[pricing]`, and `[providers.*]` are modelled; unknown sections are ignored. `ConfigPaths` and `Overrides` are explicit parameters so tests never touch the real home directory.
- `src/turn.rs`: the one text-only turn behind `run`, plus `RunOutput` (the `run --json` document; field order is the contract, so it is a struct, not a `serde_json::Value`, whose map would sort keys) and the stderr summary line.
- `src/main.rs`: the `clap` CLI. As with Typer, `--model` is a top-level option and must come before the subcommand. Errors print `error: ...` plus a hint on stderr and exit 1; usage errors exit 2; ANSI styling only when stderr is a terminal and `NO_COLOR` is unset.
- `tests/compatibility.rs`: integration tests that pin the JSON wire format to the Python vocabulary.
- `tests/openai_provider.rs` with `tests/support/mod.rs`: a scripted HTTP/1.1 mock server (one `Reply` per connection: full status, SSE, truncated, or stalled) plus recorded fixtures in `tests/fixtures/openai/*.sse` (kept LF by `.gitattributes`).
- `tests/cli_run.rs`: runs the real binary against the mock server inside a throwaway workspace whose `HOME`/`USERPROFILE`, working directory, and environment are isolated. Use this pattern for any test of CLI output or exit codes.

When porting a module, read the Python source first. It is public at `Maetek-Technologies/agent-86` (`src/agent86/...`); its comments explain why each behavior exists, and those reasons usually belong in the Rust doc comments too.

Conventions encoded in the types that must be preserved:

- Serde JSON is the wire format and must match the Python field names and values exactly. Enums use `rename_all = "snake_case"` (e.g. `end_turn`, `tool_use`). Optional/empty fields use `skip_serializing_if` plus `default` so absent and empty round-trip the same way Python does. `ToolResult.ok` defaults to `true` when missing.
- `ModelRef::parse` splits `provider:model` on the **first** colon only and trims whitespace (`local:model:variant` gives model `model:variant`).
- When a provider returns malformed tool-call JSON, the raw string is kept under `INVALID_TOOL_ARGS_KEY` (`"__invalid_json__"`) in `ToolCall.arguments` rather than failing; read it back with `ToolCall::invalid_arguments()`.

## Working rules for this port

- Add behavior only together with tests that make the compatibility contract explicit. Compare observable behavior; don't copy Python implementation details. Design around Rust async ownership and cancellation instead of literally reproducing Python's threads and event loops.
- Work in small vertical slices that follow the roadmap order. Don't broaden features beyond what the Python version does.
- `run --json` must emit exactly one JSON object on stdout with errors on stderr. Keep the field names `session_id`, `output`, `steps`, `usage`, `turn` from the start.
- Default tests must need no credentials or network: use recorded fixtures (e.g. SSE) and local mock servers. Live provider smoke tests are opt-in only and stay out of normal CI.
- Secrets come from environment variables or the OS keyring only. They must never appear in config files, logs, errors, tests, or commits, and secret-shaped values should be redacted in error and trace paths.
- Don't claim performance improvements without benchmarks.
