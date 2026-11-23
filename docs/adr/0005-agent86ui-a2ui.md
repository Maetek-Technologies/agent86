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

   Revised 2026-11-22, after 1.2.0 shipped: the terminal renderer moves up to **1.2.2**, right after the ACP extension, so agent86UI reaches the CLI's interactive interface sooner. Scheduled jobs become 1.2.3 and MCP vault secrets 1.2.4.

   Revised again 2026-11-22: the terminal renderer goes first, as **1.2.1**, and the ACP extension with its text fallback becomes **1.2.2**. Scheduled jobs (1.2.3) and MCP vault secrets (1.2.4) stay put.

### Questions still open, for the first slice

2. **The password-style field** (answered 2026-11-22). It is `TextField` with `"variant": "obscured"` (the basic catalog's `variant` enum is `longText`, `number`, `shortText`, `obscured`). The core refuses it in slice 2; the protocol crate stays policy-free.
3. **The spec's finer points** (partly answered 2026-11-22 from the specification's text). The `action` fields are `name`, `surfaceId`, `sourceComponentId`, `timestamp` (an RFC 3339 date-time), and `context`, all required, plus `userMessage` and `metadata`. The agent-side error codes are `VALIDATION_FAILED`, `UNALLOWED_PARENT`, `UNALLOWED_CHILD`, `INTEGRITY_ERROR`, and `INVALID_FUNCTION_CALL`; `EXPRESSION_ERROR` and `EXECUTION_ERROR` are the renderer's. Still to read for the headless runtime: the `DynamicBoolean` coercion table and the regex dialect's limits.
4. **The model's quality** (first measured 2026-11-22 on a local 27B model, see the ninth amendment, and on seven hosted models the same day, see the eleventh). Settled 2026-11-22 by the maintainer: `auto` means on wherever a renderer is, for every model, since every model measured drew a valid surface within two calls and most within one; no list of models is kept, and `[ui] agent86ui = "off"` turns it off. Measure, on the providers people use (including a local ~27B model), how often `render_ui` validates on the first try and after one correction. If smaller models fail often, `auto` may need to mean "on for models known to manage it", as the reasoning and sampling tables already single out models.

### Slices

1. **`agent86-ui`** (1.2.0), in two parts: (a) the message types, catalogs, the validator, and the conformance fixtures (done 2026-11-22); (b) the headless surface runtime (data model, bindings, templates, checks, the basic catalog's functions, building actions), which answers the rest of question 3 (done 2026-11-22; see the second amendment).
2. **Core and `sdk`** (1.2.0): the feature, `render_ui`, the prompt section, `TurnEvent::Ui`, the blocking path through the prompt queue, redaction and guardrails, the trace kinds, the config, and replay on resume. Tested against the mock server with recorded `render_ui` calls; question 4 is measured here, live and opt-in. Its first part, 2a, settled completeness and ran the validator suites (done 2026-11-22; see the third amendment); 2b put the tool, the prompt section, the event, and the builder in place (done 2026-11-22; see the fourth); 2c the redaction, guardrails, policy, and the `ui_render` trace kind (done 2026-11-22; see the fifth); 2d the blocking wait (done 2026-11-22; see the sixth); 2e actions after the turn (done 2026-11-22; see the seventh); 2f replay on resume and `Agent::surfaces` (done 2026-11-22; see the eighth); 2g the docs and the first measurement of question 4 (done 2026-11-22; see the ninth); 2h the validator's remaining rules (done 2026-11-22; see the tenth).
3. **agent86-desktop**, in its own repository, on the published 1.2.0 (its exact pin of `agent86-core` moves from `=1.1.0`). agent86-web follows on 1.2.2.
4. **The ACP extension and its text fallback** (1.2.2; planned as 1.2.1 until 2026-11-22).
5. **The terminal renderer** (1.2.1; planned as 1.2.4, then 1.2.2, until 2026-11-22), in five parts (see the eleventh amendment): 5a, the config and the core entry points (done 2026-11-22); 5b, drawing (done 2026-11-22); 5c, the blocking wait (done 2026-11-22).
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

## Amendment: completeness, and the validator suites (2026-11-22)

Slice 2a ran the A2UI repository's `validator_v1_0` and `message_processor_v1_0` suites (`conformance/core/`, converted to JSON like the others) through a strict `Runtime`, which settles the question the second amendment left open.

- **Completeness is strict by default, as in the reference SDKs.** `agent86_ui::Rules` is their `ValidationConfig` without its version and message filters: `allow_missing_root`, `allow_dangling_references`, and `allow_orphans`. `Rules::STRICT` is the default, so a missing `root`, a dangling child reference, or a component nothing shows is an error; `Rules::RELAXED` makes each a warning, for a renderer drawing a stream that is completed later. A cycle or a broken composition rule is an error either way. `Validator::set_rules` and `Runtime::set_rules` choose them; a suite step's `validationConfig` maps onto them, a rule it leaves out being strict.
- **`render_ui` uses strict rules with orphans allowed.** A2UI 1.0 has no message that removes a component, so a component replaced as a child (a loading placeholder swapped for content, as the specification's own `incremental-dashboard` example does) stays on the surface with nothing showing it, and only `deleteSurface` clears it. Under strict orphans a model that updates a surface across two calls would get an error it cannot fix. So a missing `root` and a dangling reference are errors the model must fix, and an orphan comes back as a warning in the tool result.
- **Messages use the reference SDKs' phrases**, still worded for a model: "Missing root component", "Dangling reference: … references non-existent component 'x' in field 'children[1]'", "is not reachable from 'root'", "Self-reference detected", "Circular reference detected", "Duplicate component ID", "Unknown component type 'X' … use one of: …", "Unrecognized function 'f' … use one of: …", "must be a valid UAX #31 identifier", "Unsupported protocol version", "Missing required version field", "action 'beginRendering' is not supported in protocol version 1.0", and type errors as "surfaceId must be a string, not 12345". A missing property names the component type ("\"text\" is a required property of Text").
- **Fixes the suites found:** an array of `ComponentId`s in a catalog is a child list; a catalog's `$ref` fragments are percent-decoded (a non-ASCII component name); the version is checked without a catalog (`"v99.0"` was accepted when no catalog's schema ran); `deleteSurface` of a surface that does not exist changes nothing (a warning, where it was an integrity error), as in the reference SDKs; a duplicate id in one message is an `INTEGRITY_ERROR`; a function name must be an identifier (system functions such as `@index` aside); an unknown function in an action's `functionCall` is named rather than reported as "matches none of the forms". `Runtime::capabilities` gives `renderer_capabilities.json`.
- **Results:** `validator_v1_0` runs 43 of its 50 cases and `message_processor_v1_0` 43 of its 84 (one case for 0.9.1 is outside the 1.0 filter), all passing. The rest are listed as unsupported with their reasons, to be built before the crate is first published:
  - the recursion and depth limits (component nesting 50, `functionCall` depth 5, data model depth), 6 cases;
  - RFC 6901 syntax of binding and `updateDataModel` paths beyond the leading `/`, 2;
  - a surface with no default `catalogId`, its components and calls naming their own catalogs (`CatalogError`), 10;
  - a function call naming its own `catalogId`, schema-checked against that catalog, 10;
  - a component's bare `catalogId` beside `@catalogId`, 10;
  - component `metadata` and `@metadata`, 3;
  - an unknown function nested in a system function's arguments, named, 1.

  Left out for good: the reference SDKs' own renderer error code `RECURSION_LIMIT_EXCEEDED` (outside the specification's enum), agent functions (left out of the first version above), and 0.9 data models, 5 cases in all.

## Amendment: the tool, the event, and the builder (2026-11-22)

Slice 2b put `render_ui`, its prompt section, `TurnEvent::Ui`, and `AgentBuilder::ui` in place. Where it differs from the decision above:

- **`agent86ui` is a default Cargo feature of `agent86-core`**, with `agent86-ui` an optional dependency, like `os-keyring` and `local-embeddings`; `--no-default-features` builds without either crate's dependencies (about 20 fewer). `TurnEvent::Ui`, `sdk::Ui`, `AgentBuilder::ui`, `BuildError::Ui`, and the `agent86_ui` re-exports exist only with it, which `#[non_exhaustive]` allows.
- **`render_ui` is an ordinary registry tool, not intercepted like `delegate`.** The surfaces live on `ToolContext::ui` (a `UiSession`, shared by clones like the vault), so the call gets dispatch's redaction, scopes, and argument repair unchanged. On success the tool leaves the validated messages in its result's metadata; the loop takes them out and emits one `TurnEvent::Ui` each, after the call's `ToolCall` and before its `ToolResult`, so they never reach `state_json` or the trace (the call's arguments already hold them). It is not `parallel_safe`, so surfaces build in call order.
- **The result** is `Rendered surface <id>.` (or `Deleted surface <id>.`) per surface, then each warning; a failed batch is an error result that says nothing was drawn and lists each problem with its code and path into `messages`.
- **Sub-agents never draw:** `ToolContext::fork` drops the session and a sub-agent's specs leave the tool out, so a call it makes anyway is answered "No screen that can draw UI is attached here". Only the sdk declares a renderer in 1.2.0, so `run --agent` has none either.
- **`Ui` is built, not written as a literal:** `Ui::basic()` or `Ui::new(catalogs)`, so `on_wait` (slice 2d) and the remote-media switch can be added without breaking callers. `wait` joins the tool's schema with the blocking path.
- **The prompt section** is the general rules, then per catalog its `instructions` and a reference generated from its schemas (each component's properties with `?` for optional ones and their types, whether it accepts `@checks`, and each function's arguments and return type), added by `Harness::system_for` wherever the context may call `render_ui`.
- **Publishing:** the release workflow's `crate` job now dry-runs both crates on pull requests and, on a tag, publishes `agent86-ui` and then `agent86-core`, each skipped if its version is already up. The workspace's `agent86-ui` dependency names a version (a published crate must), which each release sets to the release's version, and the crates.io token must be allowed to publish the new crate.

## Amendment: redaction, guardrails, policy, and the trace (2026-11-22)

Slice 2c put the safety rules of the decision above in place. Where it differs:

- **The egress guardrail has no `block` mode** (`[guardrails] egress` is `off`, `warn`, or `redact`). In `redact`, every string in a batch is redacted as the model's text is, before validation, so the surface, the event, and the runtime only ever hold the redacted text. In `warn`, the loop already records findings in a `render_ui` call's arguments (`egress_tool_args`, as for every tool), so nothing is added. The vault's redaction runs first in every mode, and `${secret:NAME}` is never expanded in a surface (`render_ui` does not `expands_secrets`).
- **Refused, with a result the model reads** (each a `REFUSED at <path into messages>: <why>` line, nothing drawn):
  - a `TextField` with `"variant": "obscured"`, pointing at `request_secret`;
  - an `Image`, `Video` (`url` and `posterUrl`), or `AudioPlayer` URL that is not a literal string, or is anything but a `data:` URL or a file in the workspace (resolved through `SandboxPolicy::resolve_within`; a path, relative to the workspace, that the program serves), unless `Ui::allow_remote_media` allows `https` too. The rule applies by component name, so a custom catalog's `Image` is held to it as well;
  - `openUrl` with a literal URL that is not `http` or `https`. A bound or computed URL cannot be checked before the renderer resolves it, so showing the URL and checking its scheme before opening it is the renderer's obligation, which `docs/agent86ui.md` will state;
  - `callRendererFunction` and `agentFunctionResponse` messages. `callAgentFunction` comes from the renderer, so it is refused on the way in (slice 2e).
- **The prompt section states these rules**, the media one as the remote-media switch has it, so a model learns them before it fails them.
- **The remote-media switch is `Ui::allow_remote_media()` in `sdk`** in 1.2.0; `[ui] agent86ui_remote_media` arrives with the first surface that reads config (1.2.1), as decided at slice 2's planning.
- **The trace:** `ui_render` carries the surfaces a batch names, its component count, its verdict (`rendered`, `invalid`, or `refused`), and the problems' codes. The components never appear in it. Like every tool call, though, the `tool_call` event records the call's arguments (after `redact_event`), so the batch the model wrote is in the trace there, as it is in the session. `ui_action` arrives with the actions (slices 2d and 2e).

## Amendment: the blocking wait (2026-11-22)

Slice 2d built `wait`. Where it differs from the decision above:

- **The loop waits, not the tool.** A call runs before the loop emits its messages, so a tool that waited would ask the program for an answer before the program had anything to draw. The tool marks a waiting call in its result's metadata, and the loop, once it has emitted the call's `TurnEvent::Ui` messages and recorded `ui_render`, asks (`Harness::ui_wait`) and makes the answer the call's result. So calls after a waiting one in the same step run before the user answers; the next step waits for it. The prompt says to use `wait` for a form needed before going on.
- **The question** goes through `ApprovalGate::queue` as a new `QuestionKind::Ui` (`"ui"` to a watcher), one at a time with approvals and secrets, and is never asked once the turn is cancelled.
- **The answer:** `sdk::Ui::on_wait(Fn(&UiWait) -> Option<Action>)`, where `UiWait` names the surfaces the call drew that are up. An action must be for one of them and name a component it has (`UiSession::check_action`), or the result says the answer could not be used. Its text, `[ui action] <name> on <surfaceId>: <context as JSON>` (plus the event's `userMessage`), goes through the vault's redaction and the ingress guardrail (in `block` mode an injection refuses it) and follows the call's own `Rendered surface ...` line as the result. A dismissal (`None`) or a cancel is a failed result saying the surface stays up and a later action comes as a user message.
- **Without `on_wait`,** a waiting call is refused before anything is drawn, and the prompt section leaves `wait` out.
- **The trace:** `ui_action` carries the surface, the action's name, the source component, and `waited: true`, never the context's values.

## Amendment: actions after the turn (2026-11-22)

Slice 2e built the non-blocking path:

- **`Agent::ui_action(action)` and `ui_action_with_events`** run a turn whose user message is the action's text (`[ui action] <name> on <surfaceId>: <context as JSON>`, plus the event's `userMessage`), the same text a waiting call returns. That text is the only mark that a message was an action, so `state_json` is unchanged, and a surface (or Python) shows it as the text it is. As a user message it goes through the vault's redaction and the ingress guardrail like typed input; in `block` mode an injection refuses the turn before any model call.
- **Checked first** (`UiSession::check_action`): the agent has a renderer, the surface is up, and it holds the component the action names; otherwise `RunError::UiAction` (a new `#[non_exhaustive]` variant, behind the feature), which names what was wrong and never the values, and no turn runs. It is recorded as `ui_action` with `waited: false`.
- **`callAgentFunction` cannot arrive:** the sdk takes only an `Action`, so a renderer's agent-function call has no way in, which is how the first version leaves agent functions out. ACP's `_com.maetek.agent86/a2ui/action` (1.2.2) will refuse one explicitly.
- **Actions during a turn** are the program's to hold until the turn ends, as typed input is; `Agent` takes `&mut self` per turn, so it cannot interleave them.

## Amendment: replay on resume, and the snapshot (2026-11-22)

Slice 2f built resume. Where it differs from the decision above:

- **Replay reads the steps, not the messages.** `state_json`'s steps keep every call with its result, including those compaction summarized out of the messages, so those surfaces come back too; only a session whose steps were trimmed loses them. A call counts as drawn when its result is ok or opens with what the tool writes when it draws (`Rendered surface`, `Deleted surface`, `Applied.`), since a waiting call the user dismissed reports a failure yet drew. Each drawn call is replayed in order through the same redaction, policy, and validation as when it ran (an array the model sent as a JSON string is parsed, as the registry repaired it), into a fresh runtime. So a workspace file a surface showed that has since gone, or a vault that no longer holds a value, can leave a surface out or show it differently; that is accepted with the rest of what replay loses.
- **The program gets the result as `Agent::surfaces()`** (agreed when slice 2 was planned): each surface up now as the one `createSurface` that draws it as it is (its catalog, `sendDataModel`, its components, and its data model), in surface id order, from the new `agent86_ui::runtime::Runtime::snapshot`. Replay runs when `AgentBuilder::session` resumes a session and in `Agent::with_conversation`; a program that reopens draws `surfaces()`. Nothing new is stored, and `state_json` is unchanged.

## Amendment: docs, and the first measurement of question 4 (2026-11-22)

Slice 2g wrote `docs/agent86ui.md` (with the renderer's obligations: a distinct frame, showing and checking an `openUrl` URL before opening it, workspace media kept inside the workspace) and the opt-in live check, `tests/agent86ui_live.rs`: five tasks a model would plausibly answer with a surface (a choice, a form with a date and a check box, a confirmation, a slider, a list template), each in a fresh workspace with only `render_ui` offered and four steps, counting how often the first call validates and how often one of the first two does.

**First measurement, `ollama:qwen3.8:27b-mlx` (local, about 27B), one run of five tasks:**

| | drew a surface | valid first try | valid within one correction |
|---|---|---|---|
| Before the fixes below | 5/5 | 0/5 | 2/5 |
| After them | 5/5 | 2/5 | 4/5 |

After the fixes every task had a valid surface within three calls. What the first run's failures showed, and what changed:

- **A component with no `component`** was reported once per branch of the catalog's `oneOf` (18 identical lines). Identical branch complaints are now said once (`agent86-ui`'s error narrowing).
- **`weight?: Weight`** in the prompt's reference left the model to guess (it wrote `"bold"`). The reference now resolves a catalog's own `$defs` (`weight?: number`).
- **`context` beside `event`** in a button's action, twice. The prompt now says `context` goes inside `event`.
- Still seen after the fixes: `createSurface` without `surfaceId`, and an enum value the component does not take; both are fixed on the first correction.

Five tasks and one run are too few to settle question 4, and the results vary between runs. They are enough to keep `auto` as it is for now: a local 27B model gets there with the validator's feedback, at the cost of a correction or two. The question stays open until the hosted models embedders use are measured too (`AGENT86_TEST_UI_MODELS` takes a list), before agent86UI is turned on by default anywhere a user did not ask for it (the terminal interface, 1.2.1, then the ACP extension, 1.2.2).

## Amendment: the validator's remaining rules (2026-11-22)

Slice 2h built what the third amendment listed as missing before the crate is first published. `validator_v1_0` now runs all 50 of its cases, and `message_processor_v1_0` 74 of its 84, all passing. Built:

- **The reference SDKs' limits:** values nested more than 50 deep (`MAX_DEPTH`), components more than 50 below `root`, and function calls nested more than 5 deep in each other's arguments (`MAX_CALL_DEPTH`), checked over the raw message before its schema. A cycle and an exceeded limit carry `RECURSION_LIMIT_EXCEEDED` (`Code::RecursionLimitExceeded`), the reference SDKs' code, where a cycle was `VALIDATION_FAILED`; the specification's renderer-error schema leaves codes other than its three validation ones open.
- **Path syntax:** every `@path` and `path` string must be a JSON Pointer, relative or absolute (a `~` escapes `0` or `1`), as the reference SDKs' relaxed pattern says.
- **Catalogs:** a component or call that resolves to no catalog the renderer has, because the one it names is not supported or it names none on a surface with no default, is `CATALOG_NOT_FOUND` (`Code::CatalogNotFound`), where it was `VALIDATION_FAILED`. A surface with no default catalog works as long as each component names one.
- **A call resolves to its own `catalogId`, else the surface's default,** not its component's catalog, as the specification says. The component's schema knows only its own catalog's functions, so a call into another catalog is replaced by a binding for that check and checked against its own catalog's `anyFunction` (`SchemaSet::check_function_call`). Every call is checked that way, so a call where a component's schema types nothing (inside a list of objects) has its arguments checked too; what the component's check already said is not said twice, and a generic "matches none of the forms" gives way to the call's own error. An action's `functionCall`, where a binding cannot stand, is left to the component's check.
- **Legacy envelope keys:** as the evolution guide says renderers accept during the 1.0 transition, a bare `catalogId` the component's schema in the surface's default catalog does not declare routes the component like `@catalogId`, and a bare `metadata`, `accessibility`, or `checks` its schema in its own catalog does not declare is that `@` key; a component with both spellings is refused. A property declared inline beside `id` in the standard shape is the envelope's (`Catalog::declares_own`). The batch is checked, applied, and typed in its `@` form, so `render_ui` emits only `@` keys, as agent SDKs must, even when a model writes the old ones.
- **Metadata:** `createSurface`'s `metadata` is kept (`Runtime::surface_metadata`, and in `Runtime::snapshot`); a component's `@metadata` was already kept with the component.
- **Errors said once:** a narrowed `oneOf` branch's "unevaluated properties" complaint stands only when nothing else failed in the same place.

Still left out, with reasons in the harness: agent functions (four cases, left out of the first version), 0.9 data models (two), a catalog component schema with `additionalProperties: false`, which rejects the envelope's own keys under the specification's schemas as composed (one), and a case whose stub catalogs need a permissive stand-in the harness does not build (one).

## Amendment: the terminal renderer's plan, and hosted models (2026-11-22)

Slice 5, the terminal renderer (1.2.1), was planned with the maintainer before any of its code. Where it differs from the decision above, or settles what it left open:

- **Five parts:** 5a, the config and the core entry points; 5b, drawing; 5c, the blocking wait, which turns `render_ui` on in the interface; 5d, actions after the turn; 5e, resume, the transcript view, and the docs.
- **The interface keeps its own renderer.** `App` holds an `agent86_ui` `Runtime` fed by the `TurnEvent::Ui` messages, as an embedder's renderer would be, so what the user types and presses never touches the agent's copy in `UiSession`, and the state machine stays testable with synthetic events.
- **The look:** a magenta heavy-line frame titled `agent86UI · drawn by the agent · <surfaceId>`, with no numbered choices, so it never looks like an approval or one of agent86's own panels (which are cyan and rounded).
- **Where it is:** the newest surface that is up stays in the live region above the input box, capped at about 40% of the screen like the reply's tail; the others collapse to one line. When a surface is answered, deleted, or replaced, a static copy of its last state goes into scrollback.
- **Keys:** Alt+Up focuses a surface while idle. Inside one, Tab and Shift+Tab or Up and Down move between inputs, Space toggles a check box or picks a choice, Left and Right move a slider, typing edits a text field, Enter presses the focused button, and Escape goes back to the input box (or dismisses a waiting surface).
- **`openUrl`** shows the URL and asks y/n, then opens it with the system opener, `http` and `https` only.
- **The renderer is attached after the terminal starts, not in `setup::assemble`.** `setup::attach_renderer` builds the `UiSession` from the config (the basic catalog, the egress guardrail's mode, `[ui] agent86ui_remote_media`), offers `render_ui` through `Harness::attach_ui` (which `sdk::AgentBuilder::ui` now uses too), and replays a resumed session's surfaces. The interface calls it once its terminal is up, so the plain loop, including the fallback when the terminal cannot start, never has the tool to take back, and `assemble`'s signature (shared with `run` and ACP) is unchanged. `[ui] agent86ui = "off"` leaves it out.
- **The waiter is set later.** `UiSession::set_waiter` installs the answer to a waiting call once the interface's event channel exists, as the vault's `set_prompt` does for secrets; `can_wait`, and so the prompt's `wait` rule, follow it per request.
- **One entry point for actions after a turn.** `Harness::accept_ui_action` checks an action, records `ui_action` (`waited: false`), and gives the text the turn runs on; `sdk::Agent::ui_action` and the interface both use it and run the text as a plain turn.
- **Config:** `[ui] agent86ui` (`auto` or `off`, default `auto`) is in `trust::TRUST_FREE`; `[ui] agent86ui_remote_media` (default `false`) needs trust. `run`, the plain loop, and ACP read neither.

**Question 4, hosted models** (2026-11-22, `tests/agent86ui_live.rs`, one run of the five tasks each, through OpenRouter):

| Model | drew a surface | valid first try | valid within one correction |
|---|---|---|---|
| `anthropic/claude-sonnet-5.5` | 5/5 | 5/5 | 5/5 |
| `google/gemini-3.8-flash` | 5/5 | 5/5 | 5/5 |
| `anthropic/claude-haiku-5.5` | 5/5 | 3/5 | 5/5 |
| `moonshotai/kimi-k3.1-pro` | 5/5 | 2/5 | 5/5 |
| `qwen/qwen3.8-27b` | 5/5 | 2/5 | 5/5 |
| `openai/gpt-6.2-sol` | 5/5 | 1/5 | 5/5 |
| `deepseek/deepseek-v4.1-flash` | 5/5 | 1/5 | 3/5 |

Every first failure of `gpt-6.2-sol` was a hyphenated component id (`submit-label`), which the prompt does not yet rule out; `deepseek` once put a value where a child id goes. The check reported "no render_ui call" in place of the first error for several models, a fault in its reporting. Settled the same day, before 5c turned `render_ui` on: `auto` means on for every model wherever a renderer is (question 4). After 5c's two fixes (the prompt's id rule, and the check reporting each task's first problem), a second run gave `gpt-6.2-sol` 5/5 first try; most first failures left for `claude-haiku-5.5`, `kimi-k3.1-pro`, and `qwen3.8-27b` were `wait: true` sent to a renderer with no waiter (the check declares none, yet the tool's schema still offers `wait`), which the interface, having one, would accept.

Trying 5c at a terminal (`qwen3.8:27b-mlx`) found two more things, fixed in it:

- **A waiting surface with nothing to press.** The model drew a plan picker with no button and waited, so the user could only dismiss it: in A2UI only an event action sends an answer, and choosing an option only changes the data model. `render_ui` now refuses a `wait: true` batch when a surface it creates or changes would have no Button whose action is an event (counting buttons drawn by earlier calls; an `openUrl` button does not count), before anything is drawn, and the prompt says every form needs one.
- **A waiting call's outcome comes first.** A dismissed, cancelled, refused, or unusable answer used to follow the `Rendered surface ...` line, so the interface's one-line summary read `error: Rendered surface plan.`; the outcome now comes first, and replay finds the drawing line wherever it is, so sessions written either way replay.
- **A field beside the message's kind is moved inside it.** The same model's next first call put `surfaceId` beside `createSurface`, the mistake the first measurement saw most. Such a message is never valid as written, so `agent86ui::tidy` moves every field beside the single kind into its body when the body lacks it, the result notes each move so the model learns, and replay applies the same move. The prompt now also carries one complete example call (a choice with a Submit button), checked against the validator by a test.
- Also: a string of broken JSON where an array or object belongs is told where it broke when the call fails (`tools::unparsed_json_hint`), since the same model wrapped `messages` in a string whose brackets closed out of order.
