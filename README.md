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
- Python's system prompt (identity plus environment line), checked against text extracted from the Python source
- API keys from the provider's environment variable or, failing that, the OS keyring (service `agent86`, account = provider name), compatible with keys stored by the Python implementation
- A one-shot, text-only CLI: `agent86 --model provider:model run <goal>` streams the answer, and `run --json` prints one JSON object with `session_id`, `output`, `steps`, `usage`, and `turn`
- Formatting, linting, and tests on Linux, macOS, and Windows

`run` is a working agent for the current directory: the model can list, read, write, and edit files there, see the results, and keep going until it answers. Every write is shown as a diff and needs your approval: at a terminal `run` asks `[y/N]` for each one; piped or in CI it declines them unless you pass `--yes` (or set `--approval auto`). Python's guardrails apply too: the goal is scanned for prompt injection and personal data, answers for leaked secrets and personal data (`[guardrails] egress = "redact"` rewrites them), and tool output that looks like injected instructions is marked as untrusted before the model sees it. There is no shell or Python execution, MCP, persistent sessions, or interactive REPL yet.

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
- [x] Read-only workspace tools (`read_file`, `list_dir`)
- [x] ReAct loop with circuit breakers and continuations
- [x] Approvals and file-writing tools (`write_file`, `edit_file`)
- [x] Guardrails (ingress, egress, observation scanning)
- [ ] Sandboxed command and Python execution
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

Try it against OpenRouter. Either set `OPENROUTER_API_KEY` in your environment, or store the key once in the OS keyring:

```bash
cargo run -- keys set openrouter      # prompts with input hidden, including in Git Bash
```

Then run:

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
