# 0005. agent86UI: the model draws interactive UI with A2UI 1.0, through a tool, on every surface that can render it

## Status

Accepted — 2026-11-22 (proposed the same day, before any code; the release was settled at review, and the first slice's crate and conformance tests settle the other open questions below)

## Context

The agent can only answer in text. When it needs something structured from the user (pick one of these five options, fill in these four fields, confirm this plan), it asks in prose and parses the reply, and the user types an answer the model may misread. agent86 already has structured UI for its own questions: the approval panel, limit raises, `request_secret`, and the `/config` forms. The model cannot reach any of them, and each is built separately on every surface (the terminal, ACP, agent86-desktop, agent86-web).

[A2UI](https://a2ui.org) is Google's open protocol (Apache 2.0) for exactly this. Its 1.0, the first stable version, was announced on 2026-11-11. The agent streams declarative JSON describing UI, and the renderer draws it with its own native widgets from a *catalog* it trusts. No code crosses the wire. The parts that matter here:

- **Agent-to-renderer messages:** `createSurface`, `updateComponents`, `updateDataModel`, `deleteSurface`, `callRendererFunction`, and `agentFunctionResponse`. Each is one object carrying `"version": "1.0"` and exactly one of those keys.
- **Renderer-to-agent messages:** `action` (a button press, with its `context` resolved from the data model), `callAgentFunction`, `rendererFunctionResponse`, and `error`.
- **Components:** a flat list linked by id, one of them `root`. Values bind to a per-surface data model through RFC 6901 JSON Pointers (`{"@path": "/user/name"}`). The renderer checks input itself through `@checks`. `@accessibility` carries labels and live regions.
- **The basic catalog** (Text, Image, Icon, Video, AudioPlayer, Row, Column, List, Card, Tabs, Divider, Modal, Button, CheckBox, TextField, DateTimeInput, ChoicePicker, Slider, and formatting and validation functions) is identified by its `catalogId`. A renderer states the catalogs it supports in its capabilities.
- **Transports:** the protocol names none of its own. There are bindings for A2A (an extension, `https://a2ui.org/a2a-extension/a2ui/v1.0`), AG-UI, and MCP. The MIME type is `application/a2ui+json`.
- **SDKs:** Python, TypeScript, and Dart have agent-side SDKs. React, Angular, Lit, Flutter, and SwiftUI have renderers. Google publishes no Rust SDK or validator.
- **Incompatible with 0.9.1:** the envelopes were renamed, binding keys moved from `path` to `@path`, `theme` was removed, and 0.9 catalogs do not load on a 1.0 runtime.

On crates.io, `a2ui` (with `a2ui-base` and `a2ui-tui`, MIT, 0.3.1 from 2026-06-18, about 20 stars) is a community Rust implementation. It is renderer-side, with ratatui, egui, Slint, and other backends, and its README examples write `"version": "v1.0"`, which suggests it follows a 1.0 draft rather than the final spec.

Questions this has to settle:

- What to call it, and where the code lives.
- How the model produces UI: a tool, or blocks in its text.
- How the user's answer gets back: does it block the turn or start a new one?
- How surfaces survive a resume without changing the session database Python shares.
- What each surface (the terminal, ACP, `run`, the desktop and web apps) does with it.
- How it fits the vault, the guardrails, the trace, and project trust.

## Decision

**Ruling:** agent86 adds agent86UI: the model draws A2UI 1.0 surfaces through a `render_ui` tool, validated against the renderer's catalogs by a new `agent86-ui` crate, emitted as a `TurnEvent` and through `sdk`, and drawn by every surface that declares a renderer (agent86-desktop and agent86-web first, then ACP clients through an extension, then the terminal), with user actions coming back as the next user message, and nothing new stored.

What that means:

- **The name.** The feature is *agent86UI*, and the docs describe it as "agent86's implementation of Google's A2UI protocol". Everything on the wire keeps A2UI's own identifiers (`catalogId`s, the MIME type, the A2A extension URI, `"version": "1.0"`), because interoperating with A2UI renderers is the point. agent86 never calls itself "A2UI", "official", or Google-endorsed, and uses no Google logos. The terminal interface stays "the interface" (`src/ui/`) in the docs.
- **1.0 only.** No 0.9.1 support. agent86 is the agent and ships its own renderers, so the renderers that are still on 0.9.1 (Gemini, Compose) do not constrain it. A later 1.x is taken as additive. The README already mentions a 1.1.
- **A protocol crate of its own: `crates/agent86-ui`** (`agent86-ui` on crates.io, free on 2026-11-22). It holds no agent86 types and depends on nothing in the workspace, so a Rust program can use it without the harness. It covers:
  - serde types for both message directions, the catalog definition, and the capabilities, `#[non_exhaustive]` wherever 1.x may grow;
  - a validator that checks messages against a catalog: unknown components or functions, a missing or duplicate `root`, broken child references, `allowedParents` and `allowedChildren`, unrecognized `@` keys, a surface created twice. It returns the spec's error codes with `surfaceId`, a JSON Pointer `path`, and a `message`;
  - a headless surface runtime: the component tree, the data model (`serde_json::Value::pointer`), binding resolution, templates with `@index`, `@checks` (including the spec's restricted regex dialect and its length limits), and building an `action` with its `context`. It draws nothing. The terminal renderer, the ACP text fallback, and resume all sit on it.

  Publishing follows the `crate` job's pattern, at the workspace version. The community `a2ui` crate was considered as a dependency and not chosen: it targets renderers rather than agents, appears to follow a draft, and the validator is the part agent86 must own.
- **Checked against the spec's own artefacts.** The JSON Schemas (`specification/v1_0/`) and the cases in the repository's `conformance/` directory are copied into `tests/fixtures/agent86ui/`, with the upstream commit recorded, and run in the crate's tests. That follows the rule for Python's fixtures: compare observable behavior, never run the reference code. A JSON Schema crate is used, if at all, as a dev-dependency only, and only if its `rust-version` is within the MSRV. The runtime validator is hand-written.
- **The model draws through a tool, `render_ui`, not inline text.**
  - Its arguments: `messages` (an array of agent-to-renderer messages) and optional `wait` (a boolean, see below).
  - Why a tool:
    - It fits the ReAct loop.
    - It goes through `ToolRegistry::dispatch`, so it gets redaction, scopes, the trace, and the repair of JSON-string arguments.
    - Its result is the validator's verdict: either `rendered surface <id>` or the error list. That makes A2UI's "generate, validate, feed back" loop the loop agent86 already runs.
  - Why the schema stays shallow (`messages: array of object`): the full catalog schema is very large, and providers' strict modes cap schema size and depth. The catalog's detail goes in the prompt, and correctness is enforced by the validator.
  - It is not side-effecting, so it needs no approval.
- **Offered only where a renderer is.** `render_ui` is registered only when the surface has declared a renderer and its catalogs, the same rule `request_secret` follows. So `run`, the plain loop, piped input, and CI never see it. When it is registered, a system-prompt section, rebuilt per request like the MCP section, carries the catalogs' `instructions` and a compact component reference. Catalogs cost thousands of tokens, which is why the section is never there otherwise. Sub-agents do not get the tool in the first version; only the main loop and `run --agent`'s specialty may draw.
- **Two ways to answer.**
  - **Non-blocking, the default.** A surface stays up after the call. A user action becomes the next user message. It is sent as text the model reads (`[ui action] <name> on <surfaceId>: <context as JSON>`) and marked so surfaces show it as an action, not something typed. If no turn is running, it starts one. If a turn is running, it waits, as typed input does.
  - **Blocking, `wait: true`.** For a form in the middle of a turn. The surface is shown, and the turn waits for its first action through `ApprovalGate::queue`, as a secret prompt does: one question at a time, declined once the turn is cancelled. The action's `context` is the tool's result. Without a renderer that can answer (the hook is unset), the call fails with a result saying so.
- **Resume replays; nothing is stored.**
  - A surface is the sum of the successful `render_ui` calls in the history, and the actions after them are user messages already there.
  - So `session/load`, a desktop reopen, and `/resume` rebuild every surface by replaying those calls through the headless runtime. `state_json`, the database schema, and the trace format are unchanged, and a session that used agent86UI still opens in Python, which simply shows the tool calls.
  - Two things are lost, and accepted: values typed into a field but never submitted, and surfaces whose calls compaction summarized away. Long-lived dashboards are not a goal.
- **The core feature, `agent86ui`, in `agent86-core`.** It holds the tool, the prompt section, the blocking hook, and a new event, `TurnEvent::Ui(&agent86_ui::AgentMessage)`, emitted once per validated message. `TurnEvent` is `#[non_exhaustive]` (ADR 0004), so the event is additive for Rust callers, and the bindings' records gain a `ui` kind. The binary's matches (`RunPrinter`, `ui::app`, `acp::harness`) each get an explicit arm.
- **In `sdk`:**
  - `AgentBuilder::ui(Ui)`, which carries the catalogs and an optional `on_wait` callback (`Fn(&UiWait) -> Option<Action>`, on the turn's thread like the other callbacks);
  - `Agent::ui_action(action)`, for the non-blocking path;
  - the `agent86_ui` types, re-exported from `sdk`.

  The Python and TypeScript bindings (1.3.0, 1.3.1) bind these from their first release, so a TypeScript embedder can hand the events straight to Google's React or Lit renderer.
- **Safety, as on every path to the user.**
  - Every string in a message goes through the vault's redaction before it is emitted or recorded, and `${secret:NAME}` is never expanded in a surface.
  - The egress guardrail scans every string: in `redact` mode its values are redacted, and in `block` mode the call is refused with a result the model reads.
  - What the user enters comes back through the ingress guardrail and the vault, as typed text does.
  - Secrets are not collected through a surface: the validator refuses a password-style field, and the prompt points the model at `request_secret`, so a secret's value never reaches the model.
  - Media fetches would let a surface track the user or leak data in a URL. So `Image`, `Video`, and `AudioPlayer` take a `data:` URL or a workspace path (resolved through `SandboxPolicy::resolve_within` and served by the surface) unless `https` is allowed by `[ui] agent86ui_remote_media`.
  - Before `openUrl` runs (it already needs a user gesture), the surface shows the URL, and only `http` and `https` are opened.
  - `callRendererFunction` and `callAgentFunction` are left out of the first version, and the validator refuses them.
- **The trace.** New kinds, `ui_render` and `ui_action`, in agent86's namespace (Python has no equivalent), with the surface id, the component count, and the validator's verdict, after `redact_event`. The full component tree stays out of the trace; it is in the session's tool calls.
- **Config: `[ui] agent86ui = "auto" | "off"`** (default `auto`, meaning on wherever a renderer is) **and `[ui] agent86ui_remote_media`** (default `false`).
  - `agent86ui` goes in `TRUST_FREE`: the default is already `auto`, so an untrusted project can only turn it off.
  - `agent86ui_remote_media` loosens what a surface fetches, so it needs trust.
  - `run` never reads either.
- **The surfaces, in the order they get it:**
  1. **agent86-desktop**, then agent86-web.
     - The Tauri back end maps the new event in `src-tauri/src/threads/events.rs` and sends it to the React front end.
     - Rendering uses Google's `@a2ui/react` if it supports 1.0 when the slice starts, and otherwise a thin renderer of the app's own. Styling is the app's either way, since 1.0 has no `theme`.
     - Actions go back through a Tauri command to `Agent::ui_action`, or answer `on_wait` as the app's secret panel answers `on_secret_request`.
     - Workspace media is served through Tauri's asset protocol, under a tightened CSP.
     - The desktop pins `agent86-core` exactly, so it picks this up from the release that ships it.
  2. **ACP clients**, through an extension, since ACP has no generic UI content:
     - The capability is `com.maetek.agent86/a2ui`, advertised to all. A client that sets it in its capabilities lists its `supportedCatalogIds`.
     - Messages go only to such a client, as a `_com.maetek.agent86/a2ui/render` notification (`sessionId`, `messages`). Actions come back as a `_com.maetek.agent86/a2ui/action` request.
     - A blocking wait is sent through `TurnSink::call`, like a permission request.
     - Every other client gets a text fallback from the headless runtime (the surface's text, its choices, and its fields) as a message. A surface that cannot be drawn never hides the answer.
     - The names follow the namespace rule of the state, memory, and secrets extensions. The wire names keep `a2ui` because they carry A2UI messages.
  3. **The terminal interface:** a renderer for an interactive subset (Text, Row, Column, Card, List, Divider, Button, TextField, CheckBox, ChoicePicker, Slider as a number, DateTimeInput as text, media as a link) on the headless runtime and the panel machinery of `server_form.rs`. Tabs and Modal come later. The interface's levels (`full`, `summary`, `hidden`) do not apply, because a surface is a question to the user, like an approval.
  4. **`run` and the plain loop:** nothing. The tool is not registered, so the compatibility contracts are untouched.
- **Left out:** A2UI over A2A (agent86 is not an A2A agent; that would be its own ADR), AG-UI, custom catalogs (`.agent86/catalogs/`, which would need project trust and a catalog review like specialists'), A2UI from MCP servers' tool output, renderer and agent functions, and 0.9.1.

### Questions settled (2026-11-22)

1. **The release: first, as 1.2.0 to 1.2.4.** agent86-desktop matters more now than the 1.1.x patches, so agent86UI goes ahead of them. It adds to `sdk`, so it starts a minor release:
   - **1.2.0:** slices 1, 2, and 6, which is everything the desktop needs.
   - **1.2.1:** the ACP extension, slice 4, for agent86-web and editors.
   - **1.2.2 and 1.2.3:** scheduled jobs and MCP vault secrets, renumbered from 1.1.1 and 1.1.2.
   - **1.2.4:** the terminal renderer, slice 5.
   - **1.3.0 and 1.3.1:** the bindings, which therefore bind agent86UI from their first release (ADR 0004's amendment).

   The terminal renderer and the bindings do not depend on each other and can swap places.

### Questions still open, for the first slice

2. **The password-style field** (answered 2026-11-22). It is `TextField` with `"variant": "obscured"` (the basic catalog's `variant` enum is `longText`, `number`, `shortText`, `obscured`). The core refuses it in slice 2; the protocol crate stays policy-free.
3. **The spec's finer points** (partly answered 2026-11-22 from the specification's text). The `action` fields are `name`, `surfaceId`, `sourceComponentId`, `timestamp` (an RFC 3339 date-time), and `context`, all required, plus `userMessage` and `metadata`. The agent-side error codes are `VALIDATION_FAILED`, `UNALLOWED_PARENT`, `UNALLOWED_CHILD`, `INTEGRITY_ERROR`, and `INVALID_FUNCTION_CALL`; `EXPRESSION_ERROR` and `EXECUTION_ERROR` are the renderer's. Still to read for the headless runtime: the `DynamicBoolean` coercion table and the regex dialect's limits.
4. **The model's quality.** Measure, on the providers people use (including a local ~27B model), how often `render_ui` validates on the first try and after one correction. If smaller models fail often, `auto` may need to mean "on for models known to manage it", as the reasoning and sampling tables already single out models.

### Slices

1. **`agent86-ui`** (1.2.0), in two parts: (a) the message types, catalogs, the validator, and the conformance fixtures (done 2026-11-22); (b) the headless surface runtime (data model, bindings, templates, checks, the basic catalog's functions, building actions), which answers the rest of question 3 (done 2026-11-22; see the second amendment).
2. **Core and `sdk`** (1.2.0): the feature, `render_ui`, the prompt section, `TurnEvent::Ui`, the blocking path through the prompt queue, redaction and guardrails, the trace kinds, the config, and replay on resume. Tested against the mock server with recorded `render_ui` calls; question 4 is measured here, live and opt-in.
3. **agent86-desktop**, in its own repository, on the published 1.2.0 (its exact pin of `agent86-core` moves from `=1.1.0`). agent86-web follows on 1.2.1.
4. **The ACP extension and its text fallback** (1.2.1).
5. **The terminal renderer** (1.2.4).
6. **Docs** (1.2.0, extended by each later release): `docs/agent86ui.md`, the README's and the site's claims, and the plan's differences.

## Consequences

- **The model can ask structured questions** and get structured answers, on every surface that draws, from one tool, instead of each surface hand-building its own panels. agent86's own panels can move onto it later; that is not part of this decision.
- **A new published crate**, and a new protocol to follow. A2UI's 1.x releases must be tracked, and the conformance fixtures refreshed, the way the ACP schema is checked today.
- **More for the prompt.** Turning it on costs catalog tokens on every request of a turn that may draw. That is why the section appears only where a renderer is, and why question 4 is measured before `auto` is final.
- **A larger attack surface at the renderer.** Model-written UI on a trusted surface can be made to look like agent86's own approval panel. Surfaces must draw agent86UI distinctly (a frame naming the agent, never the approval styling), and the media and `openUrl` rules above are part of the decision, not polish.
- **Python compatibility is untouched.** No change to `run --json`, `state_json`, the database schema, the trace format, or the config Python reads. Python sees only an unknown tool's calls in a shared session.
- **agent86-desktop and agent86-web gain the most**, since React renderers already exist, while the terminal renderer is the largest piece of new UI code.

## Amendment: what the first slice found (2026-11-22)

Building slice 1a against the specification changed three points of the decision above. Where the text above disagrees, this section wins.

- **The validator runs JSON Schema at runtime.** A catalog is itself a JSON Schema document: each component is a schema that refers into `common_types.json`, and the envelope checks a component against `catalog.json#/$defs/anyComponent`. Checking a component against its catalog therefore needs a real JSON Schema engine, not hand-written rules.
  - `agent86-ui` depends on the `jsonschema` crate (0.63, `rust-version` 1.85) with its default features off, so it can fetch nothing.
  - Every document a reference can reach is served from memory: the vendored schemas, plus the catalog in use under the `catalog.json` placeholder. A reference to anything else fails to compile.
  - Hand-written code covers what a schema cannot express:
    - surfaces (a surface created twice, or updated before it exists);
    - unique, UAX #31 component ids;
    - composition (`allowedParents`, `allowedChildren`, `Surface` as the root's parent);
    - cycles;
    - function boundaries (`requiresUserActivation` only in an action's `functionCall`, no `agentOnly` function in a component, no agent call to a `rendererOnly` one);
    - warnings for children not defined yet, a missing `root`, and components nothing shows.
  - A model needs better errors than "matches none of the oneOf branches", so two things narrow a failure to the form the value meant: each message is checked against its own kind's definition, and inside a `oneOf` the branches the value plainly is not are set aside. Verdicts are unchanged: the conformance test checks them against the specification's.
- **Fixtures live in the crate.** The specification's schemas and basic catalog are in `crates/agent86-ui/spec/` (with the Apache-2.0 license and `UPSTREAM.md` recording the commit), and its test cases and examples are in `crates/agent86-ui/tests/fixtures/v1_0/`, not the workspace's `tests/fixtures/agent86ui/`. That way the published crate carries tests that run, which `agent86-core`'s cannot. The crate's license is `MIT AND Apache-2.0`.
  - The specification's 178 schema test cases, its example stream, and the basic catalog's 43 examples all pass. The 43 examples pass the extra rules as well.
  - The repository's top-level `conformance/` directory is a harness for renderer and agent SDKs, not schema cases, and is not used yet.
- **Surfaces may mix catalogs.** The decision implied one catalog per surface. A component is checked against the catalog it resolves to (its `@catalogId`, else the surface's default), one schema check per catalog, with error paths mapped back to the original indices. That makes mixing as cheap as not mixing.

## Amendment: the headless runtime (2026-11-22)

Slice 1b, the headless runtime (`agent86_ui::runtime`), is checked against the A2UI repository's shared conformance suites, `conformance/core/`, which every reference SDK (TypeScript, Python, Dart, Swift) runs. They are converted from YAML to JSON unchanged and replayed under the upstream harness guide's strict rules: an unknown action, op, or key fails the case. 316 of the 329 cases in the ten suites that cover the runtime run, and all pass:
- `data_model` (52), `data_context` (11), `expressions` (98), `functions` (110), `index_function` (4), `validation_result` (6), `data_deletion` (7), `data_model_pointers` (7), `reserved_keys` (10), and `actions` (11).
- The 13 left out are written for 0.9: six function cases marked `v0.9`, and seven action cases with no version that use 0.9's binding syntax (`{"path": ...}`, a literal in 1.0).
- The reference harness runs every version-less case as 0.9. Here the version-less function cases run against 1.0, and a validation result is compared with an expected boolean by its `valid`, since 1.0's validators return results where 0.9's returned booleans.

What building it settled:

- **Question 3, the rest.** The `DynamicBoolean` coercion table and the regex dialect are implemented as the specification states them, and pinned by the suites:
  - the dialect is parsed in full; refused outright are backreferences, lookbehind, named groups, `\p{}`, `\cX`, legacy octal, inline flags, and the identity escapes Unicode mode forbids;
  - the four safety rules are applied (`PATTERN_TOO_LONG`, `NESTED_QUANTIFIER`, `OVERLAPPING_ALTERNATION`, `ADJACENT_OVERLAP`);
  - a pattern that passes is translated into `fancy-regex` syntax with the dialect's meanings spelled out (ASCII `\d`, `\w`, and `\b`; `^` and `$` only at the ends), with a backtracking limit as a last defense.
- **Formatting is `en-US`, written to match `Intl`.** Numbers and currency round half away from zero on the shortest decimal form, with grouping and currency symbols. Dates use TR35 tokens on the wall-clock time as written; a date that does not exist (`2026-02-30`) gives `""`. Plurals check the explicit `zero`, `one`, and `two` forms first, as every reference SDK does. A locale option is left for later.
- **New dependencies:** `fancy-regex` (0.20, `rust-version` 1.66, already in the tree through `jsonschema`) and `jiff` (already a workspace dependency).
- **The runtime applies what the validator accepts, and evaluates it.** A batch that passes validation is applied to a copy of the surfaces and every touched surface is resolved. An evaluation error that does not depend on data (a template that does not parse, `@index` outside a list, a function no catalog defines) rejects the batch with `EXPRESSION_ERROR`, so the model hears about it in the tool result. A data model write the path does not allow rejects it too. The validator's `Code` gained `ExpressionError` and `DataError`.
- **Embedders can implement functions.** `Runtime::register` adds a local function (its `allowedCallers` and gesture rule as a catalog would declare them). It may read the data model and ask for writes, which apply when the action that ran it finishes and are dropped when it ran from a binding. A failure is an `EXECUTION_ERROR`. The suites rely on this, and custom catalogs will need it.
- **Nested child references.** The basic catalog's `Tabs` holds its children inside an array of objects (`tabs[].child`). `ChildRef::Items` covers that, in the validator's reachability and composition checks and in the runtime's tree. The property stays among the resolved properties, since it also carries each tab's title.
- **Left for the slices that use it:** agent function routing (`callAgentFunction` from a binding), the rest of the upstream suites (the validator's `validator_v1_0` and `message_processor_v1_0`, node resolution, RPC), and a locale.
  - The reference SDKs treat a missing root, dangling references, and orphans as errors in strict mode, where this validator warns.
  - Which of the two `render_ui` should follow is decided in slice 2, with those suites run then.
