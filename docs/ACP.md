# Using agent86 from an editor (ACP)

`agent86 acp` lets an editor drive agent86 over the [Agent Client Protocol](https://agentclientprotocol.com) (ACP). The editor starts agent86 as a subprocess and talks to it on its standard input and output; agent86 does the work in your project and reports back into the editor's own agent panel.

This is beyond the Python version, which has no ACP support. The design and its reasons are in [ADR 0002](adr/0002-acp-server.md), and the build history is in [the project plan](PROJECT_PLAN.md).

## Status

Every message exchanged in agent86's ACP tests was checked against the protocol's v1 JSON schema (285 messages on 2026-11-05, none invalid). It has **not** yet been tried with a real editor, so the editor settings below are written from the editors' documentation, not from a session that worked. If one does not match your editor's current settings, the editor's own documentation wins; and please report what you find.

## Setting it up

You need agent86 on your `PATH` (see [Install](../README.md#install)) and a model it can reach, exactly as for `run`: a `[model] default` in your config or `--model`, and its key in the environment or the OS keyring (`agent86 keys set <provider>`). An editor started from a launcher may not see your shell's environment, so the keyring is the dependable place for a key.

Test it by hand first. This should print one line of JSON, the handshake answer, and exit:

```bash
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}' | agent86 acp
```

### Zed

Add agent86 as a custom external agent in Zed's `settings.json`:

```json
{
  "agent_servers": {
    "agent86": {
      "command": "agent86",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Then pick it from the agent panel's new-thread menu. Zed has changed this setting's shape between versions (newer ones also want `"type": "custom"`), so check [Zed's external agents page](https://zed.dev/docs/ai/external-agents) for the current form. To choose the model for editor sessions, put it before the subcommand:

```json
"args": ["--model", "<provider>:<model>", "acp"]
```

### Other editors

Any ACP client works the same way: run `agent86` with the argument `acp`, in the project folder, and speak ACP on its stdin and stdout. The [client list](https://agentclientprotocol.com/get-started/clients) names the editors, plugins, and terminals that do (JetBrains IDEs, Neovim plugins, Emacs, VS Code extensions, and others). Their settings differ; the command and argument do not.

## What you get

- **A conversation per thread.** Each thread in the editor is an agent86 session, saved in the same database as every other agent86 session. A thread you saved can be reopened (`session/load` replays it), and `agent86 run --session <id>` or `agent86 --resume <id>` continue an editor session at a terminal.
- **Streaming.** The reply, and a thinking model's reasoning, stream into the thread as they are written.
- **Tool calls.** Each call shows in the thread when the model asks for it, with its arguments and what came back.
- **Permission requests.** The editor asks you before anything that changes something, with the change in front of you: the diff of a file edit, the command to be run, the arguments of anything else. Choose *Allow once* to let it run; anything else (reject, cancelling, closing the dialog) declines, and the model is told it was declined. A call that only reads is not asked about. There is no "always allow" yet.
- **Stop.** Cancelling in the editor stops the turn at the next safe point, as Escape does in the terminal interface: a running tool finishes, no new model call starts, and the thread ends as cancelled, not as an error.
- **Mode and model pickers.** agent86 offers two session config options, which editors show as selectors: the approval mode (`ask`, `auto`, `deny`) and the model. The model list is the models your config names, meaning `[model] default` and the two `[model.route]` entries, plus whichever one is in use. A model in that list that you have no key for is refused with the reason when picked. `/model provider:model` switches to any model, listed or not, and moves the picker. Changing either takes effect from the next turn and applies to that thread only.
- **The editor's MCP servers.** The MCP servers configured in the editor are started for each thread and their tools offered to the model beside agent86's own and any in `[mcp.servers.*]`. They run as you, like the configured ones.
- **The editor's files.** When the editor offers them, `read_file`, `write_file`, and `edit_file` go through the editor: the model sees unsaved changes, the approval shows the change from what is on your screen, and the write lands in the editor, which owns the file. Everything else the agent runs, such as a command or a Python snippet, still sees the files on disk.

Approvals follow the usual setting, `[guardrails] approval` (`ask` by default). With `auto`, nothing is asked except a write into agent86's own configuration or skills, which is asked about in every mode but `deny`; with `deny`, side-effecting calls are refused without asking.

## What is different from the terminal

- **Settings come from where the editor starts agent86.** The project's `.agent86/config.toml` is the one in that directory, which is normally the project folder. A multi-folder window uses the folder the editor chose.
- **The project file must be trusted.** agent86 never asks the trust question here (its input is the protocol), so a project file's riskier settings, such as MCP servers, providers, and approval changes, are ignored, with a note in the log, until the project is trusted. Open `agent86` in the project once at a terminal and answer `y` to the question it asks, or add `--trust-project` before `acp` in the editor's arguments.
- **Four slash commands, and no `@` menu.** agent86 offers `/mode [ask|auto|deny]`, `/model [provider:model]`, `/cost`, and `/tools` to the editor's command list (if the editor shows one), and answers them itself without a model turn, acting on that thread only. The rest of the terminal's commands (`/limits`, `/resume`, `/clear`, `/thinking`, `/secret`, and so on) are not available; set those in the config or the arguments. Text starting with a slash that is not one of the four goes to the model as written. The editor has its own mentions, which arrive as links to the files (the model gets the link and can read the file).
- **No secret prompt.** `request_secret` is not offered, because the protocol has no masked entry. Secrets remembered for the project in the keyring are not loaded either.
- **No update notice.**
- **One turn at a time** in a connection. A second prompt sent while one runs is refused.

## What is not built

- Terminals (`terminal/*`): commands run in agent86's own sandbox and their output comes back as text, not in the editor's terminal.
- ACP's older `modes` field (the config options below replace it), a boolean option kind, and a plan view in the editor's panel.
- Images, audio, and embedded resources in a prompt (it accepts text and file links).
- Remembered permission choices.
- Reconnection: if the editor goes away, the agent ends.

## Troubleshooting

- **Nothing happens, or the editor says the agent failed to start.** Run the test above in a terminal. `agent86` must be on the `PATH` the editor sees; if it is not, give the full path as `command`.
- **A model error appears in the thread.** The message is the one `run` would print: check the model name and the key. A key in an environment variable may not reach an editor launched from a menu; store it with `agent86 keys set`.
- **Your project's settings seem ignored.** See *The project file must be trusted*.
- **Where are the logs?** agent86 writes nothing to its standard output except protocol messages. Its notes (a memory database that could not be opened, an MCP server that did not start, an ignored project setting) go to standard error, which editors show in their agent or ACP log.
- **A tool from the editor's MCP server is missing.** The server may have failed to start (look in the log), or a tool name may already be taken by another server's.

## For developers

`tests/acp.rs` drives the real binary over a pipe as an editor would. Set `AGENT86_ACP_TRANSCRIPT=<file>` when running it to keep every message exchanged (`<pid> C>|A> <json>`, client to agent or agent to client), which is how the messages were checked against the schema in a clone of the [protocol repository](https://github.com/agentclientprotocol/agent-client-protocol) (`schema/v1/schema.json`). The module is `src/acp/`; its layout is in `CLAUDE.md`.
