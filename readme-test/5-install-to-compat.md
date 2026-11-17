## Install

```sh
npm install -g agent86
```

npm installs the native binary for your platform (it needs Node.js 18 or newer only to install and launch it), and `npm install -g agent86@latest` updates it. Or download the archive for your platform from [Releases](https://github.com/Maetek-Technologies/agent86/releases), then extract it and put `agent86` on your `PATH`. Builds are available for Linux x86_64 and ARM64 (including 64-bit Raspberry Pi OS; glibc 2.35 or newer, so Ubuntu 22.04, Debian 12, or later), macOS on Apple silicon and Intel, and Windows x86_64. The binaries are not code-signed: on macOS, run `xattr -d com.apple.quarantine agent86` once. Releases from v1.0.19 on carry SLSA build provenance (`agent86-<tag>.intoto.jsonl`), so you can check that an archive was built by this repository's release workflow with [`slsa-verifier`](https://github.com/slsa-framework/slsa-verifier): `slsa-verifier verify-artifact <archive> --provenance-path agent86-<tag>.intoto.jsonl --source-uri github.com/Maetek-Technologies/agent86 --source-tag <tag>`. To build from source instead, run `cargo install --git https://github.com/Maetek-Technologies/agent86 --tag v1.0.23` (needs Rust 1.94 or newer).

The interface and the plain loop check GitHub for a newer release at most once a day and say so when there is one; `/update` shows how to install it for the way you installed agent86, and `/update now` does it. `[ui] update_check = false` or `AGENT86_NO_UPDATE_CHECK=1` turns the check off, and `run` never checks.

To publish a release, add notes for it in `docs/releases/<tag>.md`, set the version in `Cargo.toml`, merge both, and push a `v<version>` tag. `.github/workflows/release.yml` then builds the archives and creates the release.

### Switching from the Python version

Uninstall it first (`pip uninstall agent86`, or `pipx uninstall agent86` if you installed it with pipx): both install a command named `agent86`, and whichever comes first on your `PATH` runs. Your `~/.agent86` config, sessions, memory, traces, prompt history, and keyring keys carry over as they are. [docs/PROJECT_PLAN.md](docs/PROJECT_PLAN.md) lists every way this version behaves differently.

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

## Why Rust?

agent86 began in Python and was rewritten in Rust for one native binary that needs no Python installation, fast startup, explicit typed state transitions, and a single async runtime for providers, tools, cancellation, and processes. The rewrite does not assume Python made model calls slow: model, network, tool, and I/O latency still dominate most workloads.

## Compatibility

The rewrite reproduced the Python version's observable behavior, checked by tests against fixtures produced from Python's own sources and CPython. What the two share stays compatible:

- CLI output streams (except `run`'s default, which keeps the tool calls off stdout; `run --output full` is Python's), exit codes, and `run --json`
- TOML configuration semantics (except that a project file's riskier settings wait for your trust)
- The session and memory database, the trace format, the prompt history, and the keyring layout

Changes beyond the Python version are deliberate, and each is recorded in the project plan.

