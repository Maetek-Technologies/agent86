# agent86-rs

An experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the agentic command-line harness.

The Python implementation remains the reference. This repository starts with a small vertical foundation and adds behavior only with tests that make compatibility explicit.

## Status

**One-shot text stage — not yet a usable agent.** The repository currently contains:

- Provider-neutral roles, phases, stop reasons, messages, tool calls, completions, steps, and usage accounting
- `provider:model` parsing
- Serde-based JSON representations and initial compatibility tests
- An async provider interface and an OpenAI-compatible streaming (SSE) provider for OpenAI, OpenRouter, and other compatible endpoints, with retries, timeouts, and secret redaction
- Recorded SSE fixtures and a local mock server, so provider tests need no credentials or network
- Layered TOML configuration (`~/.agent86/config.toml`, then `./.agent86/config.toml`, then `AGENT86_MODEL`, then `--model`) with the built-in OpenAI, OpenRouter, and Groq provider blocks
- Cost estimates from Python's price table, plus your own rates under `[pricing.models]`; models without a known price (including every OpenRouter model, as in Python) show `cost n/a`
- A one-shot, text-only CLI: `agent86 --model provider:model run <goal>` streams the answer, and `run --json` prints one JSON object with `session_id`, `output`, `steps`, `usage`, and `turn`
- Formatting, linting, and tests on Linux, macOS, and Windows

Turns are text only: there is no system prompt yet, and no orchestration loop, executable tools, MCP connections, persistent sessions, or interactive interfaces yet.

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
- [x] Text-only one-shot `run` and `run --json`
- [ ] Minimal ReAct loop
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

Try it against OpenRouter by setting `OPENROUTER_API_KEY` in your environment and running:

```bash
cargo run -- --model openrouter:<model> run "Say hello"
cargo run -- --model openrouter:<model> run --json "Say hello"
```

Any `[providers.<name>]` block with a `base_url` is treated as OpenAI-compatible; add `api_key_env` if the endpoint needs a key.

OpenRouter models have no built-in price. To see costs, add your model's rates (USD per million tokens) to `.agent86/config.toml`:

```toml
[pricing.models."openrouter:openai/gpt-4o-mini"]
input_per_mtok = 0.15
output_per_mtok = 0.60
```

## License

MIT, matching the reference implementation. See [LICENSE](LICENSE).
