# agent86-rs

An experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the agentic command-line harness.

The Python implementation remains the reference. This repository starts with a small vertical foundation and adds behavior only with tests that make compatibility explicit.

## Status

**One-shot, text-only model turns — not yet an agent.** The repository currently contains:

- Provider-neutral roles, phases, stop reasons, messages, tool calls, completions, steps, and usage accounting
- `provider:model` parsing
- Layered TOML configuration for model providers (`~/.agent86/config.toml`, `./.agent86/config.toml`, `AGENT86_MODEL`, `--model`)
- An async OpenAI-compatible streaming provider (OpenAI, OpenRouter, Groq, or any `[providers.<name>]` block with a `base_url`), with retries, split timeouts, and structured errors
- `agent86 run <goal>` and `agent86 run --json <goal>` for a single text-only turn
- Deterministic tests against SSE fixtures and a local mock server; no credentials needed

There are no tools, ReAct loop, Anthropic/Ollama/llama.cpp providers, persistent sessions, MCP connections, or interactive interface yet.

## Usage

```bash
export OPENROUTER_API_KEY=...   # read from the environment at call time, never stored
agent86 --model openrouter:<model> run "Summarize the Rust ownership model"
agent86 --model openrouter:<model> run --json "Summarize the Rust ownership model"
```

`run` streams the answer to stdout and prints a one-line turn summary to stderr. `run --json` prints a single JSON object with the reference field names: `session_id`, `output`, `steps`, `usage`, and `turn`. Errors go to stderr with exit code 1, and nothing is written to stdout.

Differences from the Python reference at this stage:

- Only OpenAI-compatible providers are ported. `anthropic:`, `ollama:`, and `llamacpp:` fail with a clear error.
- API keys come from the environment variable named by `api_key_env`; the OS keyring is not consulted yet.
- There is no price table, so `cost_usd` is `0.0` and the turn summary says the model is unpriced.
- The turn sends only the user's goal; there is no system prompt.
- Session IDs are ephemeral, and `--session` is rejected rather than silently starting a new session. `--yes` is accepted and ignored because there are no tools.
- A stream that ends with no `finish_reason` and no `[DONE]` is reported as truncated, and an `error` object inside a stream is reported as a provider error. The reference treats both as a successful turn.

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
- [x] Provider fixtures and a mock-server test harness
- [x] OpenAI-compatible streaming provider
- [x] One-shot, text-only `run` and `run --json`
- [ ] Differential tests against the Python implementation
- [ ] Ollama streaming provider
- [ ] Minimal ReAct loop
- [ ] Read-only workspace tools
- [ ] Guardrails, approvals, and sandboxed side-effecting tools
- [ ] Sessions, memory, and observability
- [ ] MCP transports and lifecycle behavior
- [ ] Interactive terminal interface
- [ ] Local embedding options

Each stage should be dogfooded against the Python version before the next becomes the default path.

## Development

Install Rust 1.89 or newer, then run:

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

The live OpenRouter smoke test is opt-in and never runs in CI:

```bash
OPENROUTER_API_KEY=... AGENT86_LIVE_MODEL=openrouter:<model> \
    cargo test --test live_openrouter -- --ignored
```

## License

MIT, matching the reference implementation. See [LICENSE](LICENSE).
