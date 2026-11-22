# agent86-ui

[A2UI](https://a2ui.org) 1.0 for Rust: the message types, catalogs, and
validation an agent needs to let its model draw interactive UI.

A2UI is Google's open protocol for agent-generated UI: the agent streams
declarative JSON describing surfaces of components, and the renderer draws
them with its own widgets, from a catalog it trusts. No code crosses the wire.
This crate is the agent's side, with no dependency on the rest of agent86, where
it is the protocol layer of agent86UI
([ADR 0005](https://github.com/Maetek-Technologies/agent86/blob/main/docs/adr/0005-agent86ui-a2ui.md)).

- `Catalog`: a catalog loaded and checked against the specification's rules
  (`Catalog::basic()` is the specification's basic catalog).
- `Validator`: checks what a model wrote, a batch at a time, against the JSON
  Schemas and the rules they cannot express (surfaces, `root`, child references,
  composition constraints, function boundaries), and reports problems with the
  specification's error codes and JSON Pointer paths, worded so a model can fix
  them. `Rules` says how complete a surface must be after a batch: strict by
  default, as in the reference SDKs (a missing `root`, a dangling child
  reference, or a component nothing shows is an error), or relaxed (each a
  warning), for a renderer drawing a stream that is completed later.
- `runtime::Runtime`: the headless surface runtime a renderer builds on. It
  applies batches (validated, then applied, all or nothing), resolves a surface
  for drawing (`tree`: every binding, function call, list template, and check
  evaluated), writes what the user enters back to the data model, and
  dispatches actions on a gesture (the `action` to send the agent, or a local
  function's effects). The basic catalog's functions are implemented, with
  `en-US` formatting and the A2UI regex dialect's safety rules.
- `AgentMessage` and `RendererMessage`: the messages as typed values.

```rust
use agent86_ui::{Validator, BASIC_CATALOG_ID};
use serde_json::json;

let mut validator = Validator::basic();
let report = validator.apply(&[json!({
    "version": "1.0",
    "createSurface": {
        "surfaceId": "greeting",
        "catalogId": BASIC_CATALOG_ID,
        "components": [{"id": "root", "component": "Text"}]
    }
})]);
assert!(!report.is_ok());
assert_eq!(report.errors[0].path, "/0/createSurface/components/0");
assert_eq!(report.errors[0].message, "\"text\" is a required property of Text");
```

The specification's JSON Schemas and basic catalog are vendored (see
`spec/UPSTREAM.md`) and served from memory: validation never fetches anything.
The crate's tests replay the specification's schema test cases and the shared
conformance suites every A2UI SDK runs for its runtime, validator, and message
processor.

agent86UI is agent86's implementation of A2UI. A2UI is Google's protocol; this
crate is not affiliated with or endorsed by Google.

## License

MIT, except the vendored A2UI specification files under `spec/`, which are
Apache-2.0 (`spec/LICENSE-APACHE`).
