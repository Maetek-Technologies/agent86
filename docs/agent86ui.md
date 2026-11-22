# agent86UI

agent86UI is agent86's implementation of Google's [A2UI](https://a2ui.org) protocol, version 1.0. It lets the model show the user interactive UI (a form, a choice, a confirmation, a small dashboard) instead of asking in prose, and get a structured answer back. The model describes the UI as A2UI messages; the program the user sees draws them with its own widgets. No code crosses the wire.

A2UI is Google's protocol, and agent86 is not affiliated with or endorsed by Google. The design and every decision since are in [ADR 0005](adr/0005-agent86ui-a2ui.md).

## Where it works

| Surface | agent86UI |
|---|---|
| Programs embedding `agent86-core` (`sdk`), such as agent86-desktop | From 1.2.0 |
| ACP clients (agent86-web, editors) | Planned for 1.2.1: the `com.maetek.agent86/a2ui` extension, and a text version for other clients |
| The terminal interface | Planned for 1.2.2 |
| `agent86 run`, the plain loop, piped input, CI | Never: the tool is not offered, so their output is unchanged |

The model can only draw where a surface has declared a renderer, so `render_ui` and its part of the system prompt (which costs a few thousand tokens per request) are absent everywhere else.

## How the model draws

The model calls `render_ui` with `messages`, a list of A2UI 1.0 agent-to-renderer messages: `createSurface`, `updateComponents`, `updateDataModel`, and `deleteSurface`. A surface is a flat list of components linked by id, one of them `root`, with values bound to the surface's data model (`{"@path": "/form/name"}`). The system prompt teaches it the rules and each catalog's components and functions.

Each call is validated as a whole against the renderer's catalogs by the `agent86-ui` crate:

- **valid:** the messages are drawn, and the result is `Rendered surface <id>.` (with any warnings, such as a component nothing shows);
- **not valid:** nothing is drawn, and the result lists each problem with A2UI's error code and a JSON Pointer into `messages`, worded for the model, which fixes them and tries again.

A surface stays up after the call. Completeness is strict: a missing `root` or a child that is not defined is an error. A component left behind when another replaced it as a child is only a warning, because A2UI 1.0 has no message that removes a component.

## How the user answers

- **Later, as a message (the default).** A button's action, such as `{"event": {"name": "submit", "context": {"name": {"@path": "/form/name"}}}}`, comes back as the next user message: `[ui action] submit on form: {"name": "Ada"}`. It goes through the same redaction and input guardrail as typed text, and starts a turn.
- **At once, mid-turn (`wait`).** With `"wait": true` the turn waits for the user's first action on the surface, and that action is the call's result. Use it for a form the task cannot go on without. A waiting question is asked one at a time with approvals and secrets, and never once the turn is cancelled. If the program cannot answer, a waiting call is refused and the model asks without waiting.

## Embedding it

```rust
use agent86_core::sdk::{Agent, AgentMessageKind, TurnEvent, Ui};

let mut agent = Agent::builder()
    .provider(provider)
    .workspace(&workspace)
    // The specification's basic catalog.
    .ui(Ui::basic().on_wait(|wait| my_renderer.wait_for_action(&wait.surface_ids)))
    .build()?;

let result = agent
    .run_with_events("Help me book a meeting", |event| {
        if let TurnEvent::Ui(message) = event {
            // One validated A2UI message, already redacted: draw it.
            my_renderer.apply(message);
        }
    })
    .await?;

// Later, when the user presses a button on a surface no call was waiting on:
let result = agent.ui_action(action).await?;
```

- `Ui::basic()` declares the specification's basic catalog; `Ui::new(catalogs)` declares your own (load one with `Catalog::from_value`).
- `TurnEvent::Ui` arrives once per validated message, after the call's `ToolCall` event and before its `ToolResult`, so you have every message to draw before `on_wait` is asked.
- `on_wait` gets a `UiWait` naming the surfaces the call drew, and returns the user's `Action` (or `None` if they dismissed it). It is called on the turn's thread.
- `Agent::ui_action(action)` runs a turn with the action as the user's message. It is refused (`RunError::UiAction`) unless the surface is up and holds the component the action names.
- `Agent::surfaces()` returns every surface up now, each as one `createSurface` message that draws it as it is. After resuming a session (`AgentBuilder::session`, `Agent::with_conversation`), the surfaces are rebuilt from the history's `render_ui` calls, so a program that reopens draws these to put the user back where they were. Nothing new is saved: the session format, which the Python version also reads, is unchanged.
- The `agent86_ui` crate is re-exported from `sdk`, with its headless runtime (`agent86_ui::runtime::Runtime`) for a renderer written in Rust: it resolves every binding, function call, template, and check, writes user input back, and builds actions.
- agent86UI is the `agent86ui` Cargo feature, on by default. With `default-features = false` it is left out, along with its dependencies.

## Safety

What a surface shows reaches the user, and what they enter reaches the model, so both go through the same checks as text does:

- **Redaction.** Every string a surface shows goes through the secret vault's redaction (a secret appears as `${secret:NAME}`, never its value) and, with `[guardrails] egress = "redact"`, the egress guardrail's. An action's text goes through the vault and the ingress guardrail; in `block` mode an injection is refused.
- **No secrets through a surface.** A `TextField` with `"variant": "obscured"` (a password field) is refused, and the model is told to use `request_secret`, whose value it never sees.
- **Media.** `Image`, `Video` (and its `posterUrl`), and `AudioPlayer` load only a `data:` URL or a file in the workspace, given as a path relative to it, unless `Ui::allow_remote_media()` also allows `https`. A media URL must be a literal string, since a bound one could not be checked first. The program serves workspace files itself (agent86-desktop plans to use Tauri's asset protocol).
- **Links.** `openUrl` with a literal URL other than `http` or `https` is refused.
- **Functions.** Renderer functions (`callRendererFunction`) and agent functions are left out of this version.
- **Trust.** Sub-agents never draw; only the main loop does.

What the renderer must do itself:

- Draw agent86UI surfaces distinctly from the program's own UI, inside a frame that names the agent, and never in the style of an approval prompt. A model could otherwise draw something that looks like agent86 asking for permission.
- Before `openUrl` opens anything, show the URL and open it only if it is `http` or `https`. A URL from a binding or a function is only known when the renderer resolves it.
- Resolve a workspace media path against the workspace, and refuse anything that leaves it.

## The trace

Two kinds of event join the flight recorder (`~/.agent86/traces/trace.jsonl`):

- `ui_render`: the surfaces a call named, its component count, its verdict (`rendered`, `invalid`, or `refused`), and the problems' codes;
- `ui_action`: the surface, the action's name, its component, and whether a call was waiting.

Neither holds what a surface showed or what the user entered. As for every tool, the `tool_call` event records the call's arguments.

## What it does not do yet

- A2UI over A2A or AG-UI, A2UI in MCP servers' output, renderer and agent functions, and A2UI 0.9 are left out.
- Config: `[ui] agent86ui = "auto" | "off"` and `[ui] agent86ui_remote_media` arrive with the first surface that reads the config file, ACP in 1.2.1. In 1.2.0 an embedder turns agent86UI on with `AgentBuilder::ui`.
