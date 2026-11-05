# Contributing to agent86

Thanks for helping. agent86 is a Rust agentic CLI harness that began as a compatibility-first rewrite of the archived [Python implementation](https://github.com/Maetek-Technologies/agent86-python).

Please read the [Code of Conduct](CODE_OF_CONDUCT.md) first. Report security problems privately, as described in [SECURITY.md](SECURITY.md), not in public issues.

## Before you start

- Read [docs/PROJECT_PLAN.md](docs/PROJECT_PLAN.md): it is the source of truth for status, next steps, the roadmap, known differences from Python, and the credential policy.
- For anything larger than a small fix, open an issue first so we can agree on the approach.
- [CLAUDE.md](CLAUDE.md) documents the architecture module by module. It is written for AI assistants but is the best map of the code for humans too.

## Setup

You need Rust 1.94 or newer (the MSRV, set in `Cargo.toml`). On Windows you also need the Visual Studio Build Tools C++ workload for `link.exe`. Some MCP tests use `python3` or `python` and skip when neither is on `PATH`.

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

All three must pass before a pull request is merged. CI runs them on Linux, macOS and Windows against stable and the MSRV.

## Ground rules

- **Tests come with behavior.** Add a test that makes the contract explicit. Default tests must need no credentials or network: use recorded fixtures and the local mock servers.
- **Stay compatible with Python** where the two share a contract: CLI output streams, exit codes, `run --json` fields, TOML config semantics, and the session database, trace, prompt history and keyring formats. Read the Python source before changing such behavior. Deliberate differences are recorded in the plan.
- **Secrets** come from environment variables or the OS keyring only. Never put one in config files, logs, errors, tests or commits, and never add a test that writes to the real keyring.
- **No `unsafe` code**; it is forbidden at the crate level.
- **MSRV:** avoid newer language or std features, and check a new dependency's `rust-version`. Raising the MSRV is a deliberate change that updates `Cargo.toml`, the CI matrix and branch protection together.
- **Dependencies and actions:** `cargo deny check advisories` must pass; an exception needs a stated reason in `deny.toml`. Pin every GitHub Action to a commit SHA with its version in a comment.
- **No performance claims without benchmarks.**
- **Docs:** when a stage lands, update `docs/PROJECT_PLAN.md` and the README status and roadmap, and keep them honest about what is and is not implemented.

## Pull requests

1. Fork, then branch from `main`.
2. Keep each pull request to one small vertical slice.
3. Fill in the pull request template and say how you tested the change.
4. Make sure CI is green. Maintainers review as time allows.

By contributing you agree that your work is released under the [MIT License](LICENSE).
