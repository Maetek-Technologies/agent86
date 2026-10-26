# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A compatibility-first Rust port of [agent86](https://github.com/Maetek-Technologies/agent-86), a Python agentic CLI harness. **The Python implementation is the behavioral oracle**: the Rust code must reproduce its observable behavior (CLI output streams, exit codes, `run --json` fields, TOML config semantics, provider streaming/tool-call behavior, approvals, session/trace formats, error degradation), not its internal structure.

The project is at the **core-types stage** and is not a usable agent yet. `docs/PROJECT_PLAN.md` is the source of truth for current status, the next milestone (OpenAI-compatible/OpenRouter provider + text-only `run` / `run --json`), the staged roadmap, and the credential policy. Read it before starting feature work, and update it (plus the README status/roadmap) when a stage lands. Keep both honest about what is and is not implemented.

## Commands

```bash
cargo test --all-targets                     # all tests
cargo test --test compatibility              # just the compatibility suite
cargo test <test_name_substring>             # single test, e.g. cargo test model_refs_split
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings    # warnings are errors
cargo run -- --help
```

All three checks (test, fmt, clippy `-D warnings`) must pass. `unsafe_code` is forbidden at the crate level. MSRV is Rust 1.77 (`rust-version` in Cargo.toml, set by `thiserror` 2.x); avoid newer language/std features, and check a new dependency's `rust-version` before adding it. Raising the MSRV is a deliberate change: update Cargo.toml and the CI matrix together.

CI (`.github/workflows/ci.yml`) runs tests on Linux, macOS, and Windows against both stable and the MSRV, with fmt and clippy on stable. Windows builds need the Visual Studio Build Tools C++ workload for `link.exe`.

## Architecture

- `src/types.rs`: the provider-neutral vocabulary everything else will build on: `Role`, `AgentPhase`, `ApprovalMode`, `StopReason`, `ModelRef`, `ToolSpec`/`ToolCall`/`ToolResult`, `Message`, `CompletionRequest`/`Completion`/`CompletionDelta`, `Step`, `Usage` (implements `Add` for accumulation).
- `src/lib.rs`: re-exports `types`; new subsystems (providers, orchestration, tools, sessions, MCP, TUI) will be added as sibling modules.
- `src/main.rs`: placeholder CLI with hand-rolled arg parsing (`--help`, `--version`; anything else exits 2 on stderr).
- `tests/compatibility.rs`: integration tests that pin the JSON wire format to the Python vocabulary.

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
