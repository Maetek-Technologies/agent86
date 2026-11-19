![The agent86 logo: a robot detective in a fedora beside a seven-segment 86, on a retro badge reading AGENT 86](docs/images/agent86-logo.png)

# agent 86

[![CI](https://github.com/Maetek-Technologies/agent86/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Maetek-Technologies/agent86/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Maetek-Technologies/agent86)](https://github.com/Maetek-Technologies/agent86/releases/latest)
[![npm](https://img.shields.io/npm/v/agent86?logo=npm)](https://www.npmjs.com/package/agent86)
[![License: MIT](https://img.shields.io/github/license/Maetek-Technologies/agent86)](LICENSE)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/Maetek-Technologies/agent86/badge)](https://scorecard.dev/viewer/?uri=github.com/Maetek-Technologies/agent86)

[![Rust](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FMaetek-Technologies%2Fagent86%2Fmain%2FCargo.toml&query=%24.package%5B%27rust-version%27%5D&label=rust&suffix=%2B&logo=rust&color=dea584)](https://www.rust-lang.org)
[![Edition](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FMaetek-Technologies%2Fagent86%2Fmain%2FCargo.toml&query=%24.package.edition&label=edition&logo=rust&color=dea584)](https://doc.rust-lang.org/edition-guide/)
[![Tokio](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FMaetek-Technologies%2Fagent86%2Fmain%2FCargo.toml&query=%24.dependencies.tokio.version&label=tokio&color=463f7a)](https://tokio.rs)
[![candle](https://img.shields.io/badge/candle-0.11-ffd21e?logo=huggingface)](https://github.com/huggingface/candle)
[![SQLite](https://img.shields.io/badge/SQLite-bundled-003b57?logo=sqlite)](https://www.sqlite.org)
[![MCP](https://img.shields.io/badge/MCP-2025--11--25-blue?logo=modelcontextprotocol)](https://modelcontextprotocol.io)
[![ACP](https://img.shields.io/badge/ACP-v1-blue)](https://agentclientprotocol.com)
[![OpenTelemetry](https://img.shields.io/badge/OpenTelemetry-OTLP%2FHTTP-425cc7?logo=opentelemetry)](https://opentelemetry.io)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)](#install)

An agentic harness on the command line. It connects to remote or local models and lets them use tools and skills, and ships as one native binary. It also works as the agent inside editors and terminals that speak the [Agent Client Protocol](#use-it-from-your-editor) (ACP). Website: [maetek-technologies.github.io/agent86](https://maetek-technologies.github.io/agent86/).

![The interactive interface at startup: the AGENT 86 header, the input box with the working directory in its border, and the status line](docs/images/interface.png)

## Quickstart

```sh
npm install -g agent86
agent86 auth                                         # pick a provider and store its key in the OS keyring
agent86 --model openrouter:<model>                   # the interactive interface
agent86 --model openrouter:<model> run "Summarize this repository"
```

Any provider works the same way: `anthropic:`, `openai:`, `google:`, `groq:`, or a local `ollama:<model>` that needs no key at all. Instead of the keyring you can set the provider's environment variable, such as `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY`. Without `--model`, agent86 uses `[model] default` from your config (an Anthropic model out of the box), and `/config model` in the interface picks and saves one for you. [docs/CONFIGURATION.md](docs/CONFIGURATION.md) lists every provider's key and the config files.

By default, every file write, command, and snippet asks for your approval before it runs.

To start a local server and connect without a hosted API key, see the
[local-model walkthrough](docs/local-models.md), including Ollama context
settings and vLLM's tool-call requirements.

## What it does

- **Works on your code.** It lists, reads, searches, writes, and edits files, runs shell commands and Python in a sandbox (or a throwaway Docker container), and fetches web pages, with every side effect behind your approval.
- **Any model.** Anthropic, OpenAI, OpenRouter, Google Gemini, Groq, and local models through Ollama, llama.cpp, vLLM, or LM Studio.
- **Interactive or scripted.** An inline terminal interface with slash commands, `@file` mentions, and pickers, or `run` for one goal at a time, with `run --json` for scripts and CI.
- **In your editor.** `agent86 acp` makes it the agent in Zed and other editors and terminals that speak ACP.
- **Skills and sub-agents.** `SKILL.md` skills as in Claude Code, and specialist sub-agents that can run in parallel or in their own git worktree.
- **MCP.** Servers over stdio, streamable HTTP, or SSE, with their tools behind the same approvals.
- **Sessions and memory.** Every run is a session you can resume, and long-term memory is embedded locally.
- **Guardrails and secrets.** Prompts, answers, and tool output are scanned for injection and leaked data, and a secret you hand over in a masked prompt reaches the model only by name.
- **A flight recorder.** A redacted trace of every turn, model call, and tool call, exportable as OpenTelemetry.

The full list, with the details and the config keys, is in [docs/FEATURES.md](docs/FEATURES.md).

## Install

```sh
npm install -g agent86
```

npm installs the native binary for your platform (it needs Node.js 18 or newer only to install and launch it). `npm install -g agent86@latest` updates it, and so does `/update now` inside agent86, which also tells you when a newer release is out (`[ui] update_check = false` or `AGENT86_NO_UPDATE_CHECK=1` turns that check off).

You can also download the archive for your platform from [Releases](https://github.com/Maetek-Technologies/agent86/releases), extract it, and put `agent86` on your `PATH`. Builds are available for Linux x86_64 and ARM64 (including 64-bit Raspberry Pi OS; glibc 2.35 or newer, so Ubuntu 22.04, Debian 12, or later), macOS on Apple silicon and Intel, and Windows x86_64. The binaries are not code-signed: on macOS, run `xattr -d com.apple.quarantine agent86` once. Releases carry SLSA build provenance, and [docs/RELEASING.md](docs/RELEASING.md#verifying-a-release) shows how to check it. To build from source instead, run `cargo install --git https://github.com/Maetek-Technologies/agent86 --tag v1.0.24` (needs Rust 1.94 or newer).

### Switching from the Python version

agent86 replaced the original [Python implementation](https://github.com/Maetek-Technologies/agent86-python), which is archived. Uninstall it first (`pip uninstall agent86`, or `pipx uninstall agent86` if you installed it with pipx): both install a command named `agent86`, and whichever comes first on your `PATH` runs. Your `~/.agent86` config, sessions, memory, traces, prompt history, and keyring keys carry over as they are. [docs/PROJECT_PLAN.md](docs/PROJECT_PLAN.md) lists every way this version behaves differently.

## Use it from your editor

agent86 is compatible with the [Agent Client Protocol](https://agentclientprotocol.com) (ACP), so clients that speak it can use agent86 as their agent: Zed, the JetBrains IDEs, many VS Code, Neovim, and Emacs integrations, and terminals such as Microsoft's Intelligent Terminal (beyond Python). Point the client at the command `agent86` with the argument `acp`. Each thread is an ordinary agent86 session, so you can continue it at a terminal with `agent86 --resume <id>`, and the reverse.

- **The conversation.** The reply and a thinking model's reasoning stream in, and every tool call shows with its arguments and result.
- **Approvals.** The client asks you before anything that changes something, with the diff or the command in front of you, and your approval mode applies as usual.
- **Stop.** Cancelling stops the turn at the next safe point.
- **Saved threads.** Your agent86 sessions, terminal ones included, appear in the client's session list under the folder they ran in, and can be reopened and renamed.
- **Pickers.** The approval mode and the model can be changed from the client's selectors, including each provider's own model list.
- **Slash commands.** `/mode`, `/model`, `/thinking`, `/toolview`, `/resume`, `/secret`, `/rename`, `/cost`, `/tools`, `/skills`, and `/agents`, answered without using the model, plus `/skill`, each skill's own command, and `/agent <name> <task>`, which run as turns.
- **Sub-agents as cards.** Each delegation is one tool call card, named for the specialist and its task, listing its calls while it runs.
- **The client's servers and files.** MCP servers configured in the client are started for the thread. If the client offers it, the file tools go through it, so the model sees your unsaved changes.
- **Secrets.** A secret you enter at a terminal in the project and choose to remember is loaded into that project's threads, and the model sees only its name. The protocol forbids asking for a secret through a form, so editors have no secret prompt; since 1.0.23, a client with its own masked entry (agent86-web, for one) can hand secrets over, list and forget them, and answer `request_secret` through the `com.maetek.agent86/secrets` extension (see [docs/ACP.md](docs/ACP.md)).

It has been used by hand in Zed and in Intelligent Terminal on Windows, and every message exchanged in the test suite was checked against the protocol's published schema. It has not yet been tried in the JetBrains IDEs or the others, so please [open an issue](https://github.com/Maetek-Technologies/agent86/issues) if one behaves differently. The client's own terminals, remembered "always allow" choices, and images or audio in a prompt are not built. Setup and what differs from the terminal are in [docs/ACP.md](docs/ACP.md).

## Status

Version 1.0.24 (2026-11-17) does everything the Python version did, and more; [docs/FEATURES.md](docs/FEATURES.md) lists what is built. Next:

- [ ] 1.0.25: scheduled jobs, which the agent schedules for itself and a per-user OS timer runs with no session open, pausing at an approval instead of failing (planned, see [docs/scheduling.md](docs/scheduling.md))
- [ ] 1.1.0: vault secrets (`${secret:NAME}`) in MCP server settings (planned; the release roadmap is in [docs/PROJECT_PLAN.md](docs/PROJECT_PLAN.md))

Notes for every release are in [docs/releases](docs/releases).

## Why Rust?

agent86 began in Python and was rewritten in Rust for one native binary that needs no Python installation, fast startup, explicit typed state transitions, and a single async runtime for providers, tools, cancellation, and processes. The rewrite does not assume Python made model calls slow: model, network, tool, and I/O latency still dominate most workloads.

## Compatibility

The rewrite reproduced the Python version's observable behavior, checked by tests against fixtures produced from Python's own sources and CPython. What the two share stays compatible:

- CLI output streams (except `run`'s default, which keeps the tool calls off stdout; `run --output full` is Python's), exit codes, and `run --json`
- TOML configuration semantics (except that a project file's riskier settings wait for your trust)
- The session and memory database, the trace format, the prompt history, and the keyring layout

Changes beyond the Python version are deliberate, and each is recorded in the project plan.

## Development

Install Rust 1.94 or newer, then run:

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

To try your build, store a key once (or set `OPENROUTER_API_KEY`) and run it:

```bash
cargo run -- keys set openrouter      # prompts with input hidden, including in Git Bash
cargo run -- --model openrouter:<model> run "Say hello"
cargo run -- --model openrouter:<model>                  # the interactive interface
```

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is a one-page map of where things live, [docs/CONFIGURATION.md](docs/CONFIGURATION.md) covers providers, servers, and prices, and [docs/RELEASING.md](docs/RELEASING.md) covers releases.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues as described in [SECURITY.md](SECURITY.md).

## License

MIT, matching the reference implementation. See [LICENSE](LICENSE).
