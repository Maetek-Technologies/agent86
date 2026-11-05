# 0002. agent86 speaks the Agent Client Protocol as a server, on its own harness

## Status

Accepted — 2026-11-05

## Context

Editors such as Zed, the JetBrains IDEs, and several VS Code and Neovim extensions drive coding agents through the [Agent Client Protocol](https://agentclientprotocol.com) (ACP): the editor starts the agent as a subprocess and exchanges JSON-RPC 2.0 with it on standard input and output. agent86 had two ways in, a terminal interface and `run`, and no way for an editor to use it. Python's implementation had none either, so there is no behaviour to stay compatible with; this is an addition.

ACP is not a fit for the interactive interface's shape. The protocol is a conversation between two programs, where the editor owns the screen, the files, and the user's attention, and the agent owns the model loop. Several things the terminal does itself, such as asking before a write, showing a diff, stopping a turn, and keeping the conversation, the editor does in its own way, and the agent has to hand it the pieces.

Questions this had to settle:

- **Where it lives.** A mode of the existing binary, or a separate adapter that wraps `agent86 run`.
- **How it is built.** A protocol library, or a small hand-written layer.
- **How a turn, which blocks on the model and on the user, shares a connection with an editor that must be able to answer, cancel, and be asked in the middle of it.**
- **What the editor's files and servers are to the agent**, given the workspace jail, the secret vault, and the rule that the agent is the one that asks permission.
- **Which of the protocol's optional parts to implement**, since a capability advertised is a promise.

## Decision

**Ruling:** `agent86 acp` is a mode of the same binary that serves ACP v1 over stdio on agent86's own harness, session store, tools, and approval gate, written by hand against the protocol's published schema, advertising only what is built, and treating everything the editor sends as data.

What that means:

- **Same harness, no adapter.** A session is built by `setup::assemble_in` for the editor's working directory, saved through `SessionStore`, and run by `Harness::run_turn_events`, so an editor thread is an ordinary agent86 session (`--resume` continues it) and every guardrail, limit, trace event, and approval rule applies unchanged. The only seams added are small and optional: `assemble_in`, `ApprovalGate::call_prompt`, and `ToolContext::editor`, each with no effect when unused.
- **Hand-written, behind a trait.** The protocol layer (`Server`) speaks JSON-RPC and drives a `Backend`; `HarnessBackend` is the real one and a scripted backend tests the protocol without a process or a model. The subset is small and the repository avoids new dependencies. The official Rust crate was not evaluated in depth (its MSRV, dependency weight, and fit with agent86's threads were not measured), so this is a judgement about the size of the job, not a finding about the crate, and it can be revisited. The cost is that correctness rests on checking against the schema, which was done with a throwaway validator over a captured transcript, and on the test suite.
- **Threads, as in the interface.** A reader thread feeds one event channel; a turn runs on a scoped worker; the connection's thread pumps the channel, writing what the turn reports and routing the editor's replies. One mechanism, `TurnSink::call`, carries every request to the editor (a permission request, a file read, a file write) and every way it can end: a reply, an error, a timeout, a cancel, a closed connection. One turn runs at a time.
- **The agent asks, the editor can also check.** Side-effecting calls go to the editor as `session/request_permission` under the existing approval modes. Only choosing the one allowing option allows; every other outcome declines. The editor's own checks on a file write come on top, as the protocol says.
- **Cancel is the harness's cancel.** `session/cancel` sets the same token Ctrl+C sets, at the same safe points, and the prompt answers `cancelled` whatever the aborted work returned.
- **The editor's files are optional and honest.** With `fs/*` advertised, the file tools read and write through the editor so the model and the user see the same text; without it, nothing changes. A read the editor cannot give falls back to the disk, and a write it refuses fails the call.
- **The editor's MCP servers are the editor's choice, and their values are literal.** They are started and mounted like configured servers, but a `${VAR}` in a value from the protocol is never expanded from agent86's environment or keyring.
- **Advertise only what is built.** `initialize` says `loadSession`, the MCP transports, and nothing else; it accepts text and file links in a prompt and refuses the rest rather than dropping it. Terminals, modes, plans, remembered permissions, images, and `session/resume` are not offered.
- **Stdout belongs to the protocol.** Nothing else may print to it in this mode; notes go to stderr. A test parses every line of stdout.
- **No questions asked.** Standard input is the protocol, so `acp` never asks the project-trust question or opens a secret prompt; an untrusted project file's riskier settings are ignored, and `request_secret` is not offered.

## Consequences

- **Editors can drive agent86** with one command and argument, and a thread carries over to the terminal and back.
- **The protocol is a second surface to keep honest.** Its wire format is pinned by tests that run the real binary and by a one-off schema check; it is not yet run in CI against the schema, because that needs a schema validator dependency and the schema file vendored. If the protocol changes, `docs/protocol/v1/` and `schema/v1/schema.json` in its repository are the reference, not memory.
- **It has not been run against a real editor** at the time of this decision. Behaviour an editor does differently from the tests' stand-in, or a setting that has changed shape, will show there first.
- **Some terminal-only features are absent in an editor** (slash commands, secret prompts, the status line, the update notice); the settings they changed move to config and arguments. The docs say so.
- **The file tools gained an indirection.** With an editor, the existence checks still use the disk, so a never-saved buffer is invisible to `read_file`, and commands see the disk, which can lag an unsaved write. Both are documented.
- **An editor-supplied MCP server runs unsandboxed as the user.** That is what the protocol asks of an agent, and the same trust the user gives a configured server; the editor is the party that chose it.
- **Later ACP versions or optional parts** (terminals, `session/resume`, remembered permissions, config options) can be added behind the same `Backend` and capabilities without changing what is above.
