# agent86-rs

An experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the agentic command-line harness.

The Python implementation remains the reference. This repository starts with a small vertical foundation and adds behavior only with tests that make compatibility explicit.

## Status

**Provider stage — not yet a usable agent.** The repository currently contains:

- Provider-neutral roles, phases, stop reasons, messages, tool calls, completions, steps, and usage accounting
- `provider:model` parsing
- Serde-based JSON representations and initial compatibility tests
- An async provider interface and an OpenAI-compatible streaming (SSE) provider for OpenAI, OpenRouter, and other compatible endpoints, with retries, timeouts, and secret redaction
- Recorded SSE fixtures and a local mock server, so provider tests need no credentials or network
- A placeholder CLI that supports only `--help` and `--version`
- Formatting, linting, and tests on Linux, macOS, and Windows

The CLI cannot call a model yet. There are no orchestration loops, executable tools, MCP connections, persistent sessions, or interactive interfaces yet.

## Why Rust?

The port explores easier binary distribution, fast startup, explicit typed state transitions, and a coherent async runtime. It is not based on an assumption that Python makes model API calls slow: model, network, tool, and I/O latency will remain dominant in many workloads.

## Compatibility contracts

The Python implementation is the behavioral oracle. As features are ported, compatibility work should preserve:

- CLI output streams, exit codes, and `run --json`
- TOML configuration semantics
- Provider streaming and tool-call behavior
- Tool approval and sandbox policy
- Session persistence and trace formats
- Graceful degradation and error behavior

Compatibility tests should compare observable behavior rather than reproduce Python implementation details.

## Roadmap

- [x] Core provider-neutral vocabulary
- [x] Recorded provider fixtures and mock-server tests
- [x] OpenAI-compatible streaming provider
- [ ] Differential test harness against the Python implementation
- [ ] Ollama streaming provider
- [ ] Minimal ReAct loop and one-shot `run --json`
- [ ] Read-only workspace tools
- [ ] Guardrails, approvals, and sandboxed side-effecting tools
- [ ] Sessions, memory, and observability
- [ ] MCP transports and lifecycle behavior
- [ ] Interactive terminal interface
- [ ] Local embedding options

Each stage should be dogfooded against the Python version before the next becomes the default path.

## Development

Install a current stable Rust toolchain, then run:

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Build the placeholder executable with:

```bash
cargo run -- --help
```

## License

MIT, matching the reference implementation. See [LICENSE](LICENSE).
