# agent86-rs

An experimental, compatibility-first Rust implementation of [agent86](https://github.com/Maetek-Technologies/agent-86), the agentic command-line harness.

The Python implementation remains the reference. This repository starts with a small vertical foundation and adds behavior only with tests that make compatibility explicit.

## Status

**A working agent, short of Python's full feature set.** The repository currently contains:

- Provider-neutral roles, phases, stop reasons, messages, tool calls, completions, steps, and usage accounting
- `provider:model` parsing
- Serde-based JSON representations and initial compatibility tests
- An async provider interface and an OpenAI-compatible streaming (SSE) provider for OpenAI, OpenRouter, and other compatible endpoints, with retries, timeouts, and secret redaction
- Recorded SSE fixtures and a local mock server, so provider tests need no credentials or network
- Layered TOML configuration (`~/.agent86/config.toml`, then `./.agent86/config.toml`, then `AGENT86_MODEL`, then `--model`) with the built-in OpenAI, OpenRouter, and Groq provider blocks
- Cost estimates from Python's price table, plus your own rates under `[pricing.models]`; models without a known price (including every OpenRouter model, as in Python) show `cost n/a`
- Python's system prompt (identity plus environment line), checked against text extracted from the Python source
- API keys from the provider's environment variable or, failing that, the OS keyring (service `agent86`, account = provider name), compatible with keys stored by the Python implementation
- A one-shot CLI: `agent86 --model provider:model run <goal>` streams the answer, and `run --json` prints one JSON object with `session_id`, `output`, `steps`, `usage`, and `turn`
- An interactive interface (`agent86` with no subcommand), inline in Claude Code's style:
  - the conversation in your terminal's scrollback, a bordered input box, a spinner, a live status line, and approval panels
  - Python's slash commands with a `/` menu, `@file` mentions with path completion, and shared prompt history
  - pickers for the approval mode, the model (with the provider's live model list), and the session to resume
  - `/config model`: pick a provider, enter its key (kept in the OS keyring only after a live connection test passes), pick a model, and save it to your user or project config with your comments kept
  - `/config mcp`: add an MCP server from a pasted JSON block or by hand, test it live, save it, and use its tools at once; edit, disable, or remove servers the same way
  - answers rendered as Markdown, coloured diffs in approvals, and Ctrl+O for the full transcript with every tool call expanded
  - Esc or Ctrl+C to cancel a turn
  - Python's plain `agent86>` loop with `--plain` or piped input
- Formatting, linting, and tests on Linux, macOS, and Windows

`run` is a working agent for the current directory: the model can list, read, write, and edit files, run shell commands, run Python snippets, and fetch public web pages, see the results, and keep going until it answers. Commands run in a sandbox (workspace as the working directory, an environment with secrets removed, a timeout that kills the whole process tree, and capped output), or in a throwaway Docker container with `[sandbox] mode = "docker"`. Every write, command, and snippet needs your approval: at a terminal `run` asks `[y/N]` for each one; piped or in CI it declines them unless you pass `--yes` (or set `--approval auto`). Python's guardrails apply too: the goal is scanned for prompt injection and personal data, answers for leaked secrets and personal data (`[guardrails] egress = "redact"` rewrites them), and tool output that looks like injected instructions is marked as untrusted before the model sees it. `web_fetch` refuses private, loopback, and cloud-metadata addresses at every redirect. Every run is saved as a session in `~/.agent86/memory.db` (the same database and format as the Python implementation); `run --session <id>` continues one. A redacted flight recorder logs every turn, model call, tool call, and guardrail hit to `~/.agent86/traces/trace.jsonl`; `agent86 trace show` and `agent86 trace export` (JSONL, JSON, or OTLP JSON) read it back. MCP servers (`[mcp.servers.<name>]`: a local `command` over stdio, or a `url` over streamable HTTP or the legacy SSE transport) are started for each run and their tools offered alongside the built-ins, behind the same approvals, plus resource, prompt, and completion tools for servers that declare them; a server that drops is reconnected, and `tools/list_changed` updates the tool list mid-run. `agent86 mcp list` and `agent86 mcp tools` show what is configured and what each server exposes.

Run `agent86` with no subcommand for the interactive interface, the same agent over a conversation. Type `/` for a menu of its commands (`/model`, `/mode`, `/cost`, `/sessions`, `/resume`, `/clear`, and more); a bare `/mode`, `/model`, or `/resume` opens a picker. `@path` puts a file (or a directory listing) from the workspace straight into your message, and typing `@` offers matching paths. Escape or Ctrl+C stops a running turn at the next safe point and keeps what happened so far. Shift+Tab cycles the approval mode. `agent86 --resume <id>` reopens a saved session. Prompts are remembered in `~/.agent86/history`, shared with the Python implementation. In place of Python's full-screen interface, the interface renders inline in Claude Code's style ([docs/INTERFACE.md](docs/INTERFACE.md)). `/config model` manages providers, keys, and models, and `/config mcp` adds, edits, tests, enables or disables, and removes MCP servers while the session runs. `--plain` (or `AGENT86_PLAIN`) gives Python's plain loop. Long-term memory is not ported yet.

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
- [x] Sandboxed command and Python execution (subprocess, or Docker)
- [x] `web_fetch` with SSRF protection
- [x] Sessions (save and resume, shared with Python)
- [x] Flight recorder (`agent86 trace path|show|export`)
- [ ] Episodic and semantic memory
- [x] MCP client: stdio, streamable HTTP, and SSE transports, reconnects, `list_changed`, progress, and resource/prompt/completion tools (`agent86 mcp list|tools`)
- [x] Interactive REPL (Python's plain loop, slash commands, `@file` mentions, history, Ctrl+C cancellation)
- [x] Interactive terminal interface: inline, in Claude Code's style, with Python's commands, approvals, and management screens ([ADR 0001](docs/adr/0001-inline-terminal-interface.md))
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
cargo run -- --model openrouter:<model>                  # the interactive REPL
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
