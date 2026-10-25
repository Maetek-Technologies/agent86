# agent86-rs

An experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the agentic command-line harness.

The Python implementation remains the reference. This repository starts with a small vertical foundation and adds behavior only with tests that make compatibility explicit.

## Status

**Core-types stage — not yet a usable agent.** The repository currently contains:

- Provider-neutral roles, phases, stop reasons, messages, tool calls, completions, steps, and usage accounting
- `provider:model` parsing
- Serde-based JSON representations and initial compatibility tests
- A placeholder CLI that supports only `--help` and `--version`
- Formatting, linting, and tests on Linux, macOS, and Windows

There are no model providers, orchestration loops, executable tools, MCP connections, persistent sessions, or interactive interfaces yet.

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
- [ ] Recorded provider fixtures and differential test harness
- [ ] OpenAI-compatible and Ollama streaming providers
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
