# Architecture Decision Records

This directory records decisions where agent86 deliberately departs from the Python implementation it replaced, or chooses between designs that the Python code did not settle. Python's ADRs live in its archived repository ([agent86-python](https://github.com/Maetek-Technologies/agent86-python)) under `docs/adr/`; this directory has its own numbering.

## Convention

The format is the same lightweight MADR the Python project uses.

- **Path and naming:** `docs/adr/NNNN-kebab-title.md`, numbered from `0001` and zero-padded to four digits.
- **Required headings, in this order:** `## Status`, `## Context`, `## Decision`, `## Consequences`.
- **`## Decision` opens with a line beginning `**Ruling:**`**, which states the decision in one sentence.
- **`## Status`** is `Proposed` (a draft with a prototype, not yet binding), `Accepted`, `Superseded by NNNN`, or `Deprecated`, followed by the date it was set (for example `Accepted — 2026-10-26`).

When an ADR departs from Python, add the difference to the matching "Known ... differences from Python" list in `docs/PROJECT_PLAN.md` as well.

## Index

| ADR | Ruling |
|---|---|
| [0001 — The interactive interface renders inline, in Claude Code's style](0001-inline-terminal-interface.md) | Stage 9's interface renders inline (native scrollback plus a live region at the bottom), on crossterm, in Claude Code's visual style. It keeps Python's commands, messages, approvals, files, and cancellation, and covers every Python screen in stages. |
| [0002 — agent86 speaks the Agent Client Protocol as a server, on its own harness](0002-acp-server.md) | `agent86 acp` serves ACP v1 over stdio on agent86's own harness, session store, tools, and approval gate, written by hand against the published schema, advertising only what is built, and treating everything the editor sends as data. |
| [0003 — agent86's harness is offered as an embeddable agent API, starting inside the crate](0003-embeddable-agent-api.md) | A small builder API (`agent86::sdk`) assembles the same harness from parts the caller hands it, reading no config, environment, home directory, keyring, or terminal and keeping every safety default; first as a module of this crate, later as the public face of an `agent86-core` crate. |
| [0004 — Python and TypeScript bindings wrap `agent86_core::sdk`, Python first](0004-python-and-typescript-bindings.md) | Python bindings (PyO3, `agent86-core` on PyPI) in 1.3.0 and TypeScript bindings (napi-rs, `@maetek/agent86-core` on npm) in 1.3.1 (renumbered from 1.2.0 and 1.2.1 on 2026-11-22), thin wrappers over `sdk` alone, in the workspace at its version, released by the tag, with each language's own async API over the harness's synchronous tools. |
| [0005 — agent86UI: the model draws interactive UI with A2UI 1.0, through a tool](0005-agent86ui-a2ui.md) | The model draws A2UI 1.0 surfaces through a `render_ui` tool, validated by a new `agent86-ui` crate, emitted as a `TurnEvent` and through `sdk`, and drawn by every surface that declares a renderer (the desktop and web apps first, then ACP clients through an extension, then the terminal), with actions coming back as the next user message and nothing new stored. |
| [0006 — `ask_user`: the model asks the user its questions through a form agent86 builds](0006-ask-user.md) | agent86 adds `ask_user`, whose arguments are a short, flat list of questions; agent86 builds the form, so it is always valid, draws it wherever the user can answer (an A2UI form, an ACP extension, or text), returns the answers keyed by question id, and tells the model to ask its questions through it instead of in prose. |
