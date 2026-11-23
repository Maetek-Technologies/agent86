# agent86UI

agent86UI is agent86's implementation of Google's [A2UI](https://a2ui.org) protocol, version 1.0. It lets the model show the user interactive UI (a form, a choice, a confirmation, a small dashboard) instead of asking in prose, and get a structured answer back. The model describes the UI as A2UI messages; the program the user sees draws them with its own widgets. No code crosses the wire.

A2UI is Google's protocol, and agent86 is not affiliated with or endorsed by Google. The design and every decision since are in [ADR 0005](adr/0005-agent86ui-a2ui.md).

## Where it works

| Surface | agent86UI |
|---|---|
| Programs embedding `agent86-core` (`sdk`), such as agent86-desktop | From 1.2.0 |
| The terminal interface | From 1.2.1: see [In the terminal](#in-the-terminal) |
| ACP clients (agent86-web, editors) | Planned for 1.2.3: the `com.maetek.agent86/a2ui` extension, and a text version for other clients |
| `agent86 run`, the plain loop, piped input, CI | Never: the tool is not offered, so their output is unchanged |

The model can only draw where a surface has declared a renderer, so `render_ui` and its part of the system prompt (which costs a few thousand tokens per request) are absent everywhere else.

## How the model draws

The model calls `render_ui` with `messages`, a list of A2UI 1.0 agent-to-renderer messages: `createSurface`, `updateComponents`, `updateDataModel`, and `deleteSurface`. A surface is a flat list of components linked by id, one of them `root`, with values bound to the surface's data model (`{"@path": "/form/name"}`). The system prompt teaches it the rules and each catalog's components and functions.

Each call is validated as a whole against the renderer's catalogs by the `agent86-ui` crate:

- **valid:** the messages are drawn, and the result is `Rendered surface <id>.` (with any warnings, such as a component nothing shows);
- **not valid:** nothing is drawn, and the result lists each problem with A2UI's error code and a JSON Pointer into `messages`, worded for the model, which fixes them and tries again.

A field the model puts beside a message's kind (`"surfaceId"` next to `"createSurface"` rather than inside it) is moved inside, and the result says so. Two more shapes A2UI accepts but that would not do what the model meant are completed (from 1.2.2, [ADR 0006](adr/0006-ask-user.md)): a Button event with no `context` gets one binding every top-level value of its surface (the data model's keys and what its inputs bind), and an object that is exactly `{"path": "/..."}` inside components, which A2UI 1.0 reads as a literal (0.9.1 bound values that way), is read as the `@path` binding it was meant to be, with a note. Arguments that are not valid JSON are told where they broke, and one `}` or `]` too many is dropped and the call goes ahead, with a note; a second broken call in a row is told to build the surface in parts. A surface stays up after the call. Completeness is strict: a missing `root` or a child that is not defined is an error. A component left behind when another replaced it as a child is only a warning, because A2UI 1.0 has no message that removes a component.

## How the user answers

- **Later, as a message (the default).** A button's action, such as `{"event": {"name": "submit"}}` on a form whose data model holds `name`, comes back as the next user message: `[ui action] submit on form: {"name": "Ada"}`. It goes through the same redaction and input guardrail as typed text, and starts a turn.
- **At once, mid-turn (`wait`).** With `"wait": true` the turn waits for the user's first action on the surface, and that action is the call's result. Use it for a form the task cannot go on without. Only a Button whose action is an event sends an answer (choosing or typing only changes the data model), so a waiting call whose surface has none is refused before it is drawn. A waiting question is asked one at a time with approvals and secrets, and never once the turn is cancelled. If the program cannot answer, a waiting call is refused and the model asks without waiting.

## In the terminal

From 1.2.1 the interactive interface (`agent86` with no subcommand, at a terminal) declares a renderer, so the model can draw there. `run`, the plain loop (`--plain`, `AGENT86_PLAIN`, piped input), and ACP never offer `render_ui`, and their output is unchanged.

- **What it draws:** the basic catalog's interactive subset: Text, Row, Column, Card, List, Divider, Button, TextField, CheckBox, ChoicePicker, Slider (as a number), DateTimeInput (as text), and Image, Video, and AudioPlayer as a link. Tabs and Modal say they are not drawn in the terminal yet.
- **How it looks:** inside a magenta frame with heavy lines titled `agent86UI · drawn by the agent · <surfaceId>`, which nothing else in agent86 uses, so a surface never looks like an approval. The newest surface sits above the input box, cut to about 40% of the screen; the others are counted on one line. When a surface is answered, deleted, or replaced by a newer one, a copy of its last state goes into scrollback. The display levels (`/thinking`, `/toolview`) never hide a surface, since it is a question to you.
- **Keys:** a waiting surface takes the keys at once; otherwise Alt+Up (Option+Up) opens the live surface while no turn runs, and again goes to the next older one. Inside a surface:
  - Tab and Shift+Tab, or Up and Down, move between inputs (Up and Down go through a choice's options first);
  - Space toggles a check box or picks a choice, and Left and Right move a slider;
  - typing, Backspace, Ctrl+U, and paste edit a text field;
  - Enter presses a button, which sends its answer;
  - Escape dismisses a waiting surface, or goes back to the input box. Ctrl+C dismisses and cancels the turn.
- **Links:** a button that opens a URL shows it and asks y/n, then opens it with your system's opener, `http` and `https` only.
- **Resuming:** starting with a session, `/resume`, and `/clear` redraw the surfaces the session has up. Ctrl+O's transcript view ends with the live surface in full.
- **Settings** (`[ui]`): `agent86ui = "auto"` (the default: on for every model) or `"off"`, which a project file may set without being trusted; `agent86ui_remote_media = true` lets media load `https` URLs, and needs trust.
- **Questions from the agent** (from 1.2.2, [ADR 0006](adr/0006-ask-user.md)): when the agent needs something from you, it calls `ask_user` with a list of questions, and agent86 builds the form itself, so it always works. It is titled `agent86UI · questions from the agent` and takes the keys at once: each choice question has an "Other" option and a field for your own words, Send stays dimmed until every question not marked (optional) has an answer, Escape dismisses it (the agent is told not to ask again in that turn), and Ctrl+C dismisses it and cancels the turn. Once sent, a copy goes into scrollback. In the plain loop (`--plain`, or when the interface cannot start) at a terminal, the same questions are asked as text, one at a time: type an option's number or your own words, Enter alone skips an optional question, and `/skip` stops answering. `[ui] ask_user = "off"` (allowed in an untrusted project file) turns it off, and the agent asks in its reply as before. It does not depend on `[ui] agent86ui`, since the agent writes none of the form.

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
- **Questions (`ask_user`, from 1.2.2, [ADR 0006](adr/0006-ask-user.md)).** A `Ui` with `on_wait` and the basic catalog also gets `ask_user`, with no code of your own: when the agent asks, agent86 builds the form and sends it as `TurnEvent::Ui` messages (a `createSurface` for surface `agent86_ask_<n>`), asks your `on_wait` for that surface, and sends a `deleteSurface` once it is answered or dismissed. Return the action of its `send` button (event `answers`), as your renderer builds it; `None` dismisses. The form is not kept with the agent's surfaces (`Agent::surfaces` never lists it), and `render_ui` cannot use the `agent86_ask_` prefix. Without `on_wait`, `ask_user` is not offered.
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
- ACP clients get agent86UI in 1.2.3. An embedder turns it on with `AgentBuilder::ui`; the `[ui]` settings are the interface's.
