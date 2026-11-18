# Where things live

A one-page map for a first contribution. [CLAUDE.md](../CLAUDE.md) has the module-by-module detail and the rules each module must keep, and [PROJECT_PLAN.md](PROJECT_PLAN.md) has status, the roadmap, and every deliberate difference from the Python implementation. You do not need either to fix a bug or add a test.

## How a turn flows

1. `src/main.rs` parses the command line (`clap`). `--model`, `--sandbox`, and `--approval` come before the subcommand.
2. `src/config.rs` loads layered TOML (built-in defaults, `~/.agent86/config.toml`, `./.agent86/config.toml`, `AGENT86_MODEL`, `--model`), and `src/trust.rs` decides how much of the project's file to honour.
3. `src/setup.rs` (`assemble`) builds the harness for every surface: the provider, the jailed tool context, the sandbox, the session store, and MCP.
4. `src/agent.rs` runs the ReAct loop (`Harness::run_turn`): call the model, run the tool calls it asks for, repeat until it answers or a limit stops it.
5. Side-effecting tool calls wait on `src/approval.rs` (`ApprovalGate`), and `src/guardrails.rs` scans the goal, the tool output, and the answer.
6. The session is saved by `src/session.rs`, and the flight recorder (`src/observability.rs`) logs the turn.

## The source tree

| Path | What it holds |
| --- | --- |
| `src/types.rs` | The provider-neutral vocabulary: messages, tool calls, completions, usage. |
| `src/provider/` | One adapter per model API (`openai.rs` also serves OpenRouter, Groq, llama.cpp, vLLM, and LM Studio; `anthropic.rs`, `google.rs`, `ollama.rs`), plus retries, model lists, and context windows. |
| `src/tools/` | The built-in tools: files, search, the sandboxed executors, `web_fetch`, memory, skills, secrets, and `delegate`. `builtin_registry` in `mod.rs` lists them. |
| `src/agent.rs`, `src/turn.rs` | The loop, and the `run --json` output. |
| `src/subagent.rs`, `src/agents_def.rs`, `src/specialty_turn.rs`, `src/agents/` | Sub-agents, specialist definitions, and the built-in specialists. |
| `src/skills.rs` | `SKILL.md` discovery and parsing. |
| `src/mcp/` | The MCP client and its transports. |
| `src/memory/`, `src/session.rs` | Long-term memory and the session database (shared with Python, so its schema is fixed). |
| `src/secrets.rs`, `src/vault.rs`, `src/keys.rs`, `src/auth.rs` | Key lookup and redaction, secrets the user enters, and `agent86 keys` and `agent86 auth`. |
| `src/repl.rs` | The plain loop and the slash-command registry (`COMMANDS`) both terminal surfaces share. |
| `src/ui/` | The interactive interface. `app.rs` is the state machine with no terminal access, so tests drive it directly; `screen.rs` draws. |
| `src/acp/` | The Agent Client Protocol server for editors (`mod.rs` is the protocol, `harness.rs` connects it to agent86 sessions). |
| `src/observability.rs`, `src/otel.rs` | The trace and OpenTelemetry spans. |
| `npm/`, `site/` | The npm packages and the project website. |

## Tests

Default tests need no credentials or network (the few live tests are opt-in): providers are tested against a local mock server (`tests/support/mod.rs`) with recorded fixtures in `tests/fixtures/<provider>/`. Each area has its own file, named for it (`tests/openai_provider.rs`, `tests/mcp.rs`, `tests/repl.rs`, `tests/acp.rs`, and so on). `tests/cli_run.rs` runs the real binary in an isolated home directory; copy its pattern for any test of CLI output or exit codes. `tests/compatibility.rs` pins the JSON wire format to Python's.

```bash
cargo test --test cli_run          # one test file
cargo test model_refs_split        # one test by name
```

## Where to start for common changes

- **A new provider:** an adapter in `src/provider/` (or a section in the config defaults if it is OpenAI-compatible), wired up in `provider_for_ref` in `src/provider/mod.rs`, with a fixture and a test file of its own.
- **A new tool:** a `Tool` in `src/tools/`, registered in `builtin_registry`, with every path it touches going through `SandboxPolicy::resolve_within`.
- **A new slash command:** an entry in `COMMANDS` in `src/repl.rs`; editors have their own list in `src/acp/harness.rs`.
- **A config key:** `src/config.rs`, documented in [CONFIGURATION.md](CONFIGURATION.md) or [FEATURES.md](FEATURES.md).
- **A design decision:** an ADR in [adr/](adr/README.md).

## Other docs

- [FEATURES.md](FEATURES.md): everything that is built, and the roadmap.
- [CONFIGURATION.md](CONFIGURATION.md): keys, providers, servers, and prices.
- [INTERFACE.md](INTERFACE.md), [ACP.md](ACP.md), [subagents.md](subagents.md), [skill-commands.md](skill-commands.md), [checkpointing.md](checkpointing.md), [scheduling.md](scheduling.md): one feature each.
- [RELEASING.md](RELEASING.md): publishing and verifying a release.
