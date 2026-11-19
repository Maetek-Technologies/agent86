# 0004. Python and TypeScript bindings wrap `agent86_core::sdk`, Python first

## Status

Accepted — 2026-11-19 (proposed the same day, before any binding code; the name, the minimum versions, and `TurnEvent`'s `#[non_exhaustive]` were settled at review, and the first slice of 1.2.0 is the prototype that settles the binding crates' lints)

## Context

[ADR 0003](0003-embeddable-agent-api.md) made agent86's harness embeddable from Rust. `agent86-core` 1.1.0 is on crates.io, and `agent86_core::sdk` is its stable surface. That ADR left bindings for other languages out of scope: "Python (PyO3) and TypeScript (napi-rs) bindings over `sdk` are where most agent builders work, and are the main reason to keep the surface small and owned types only. They are a separate decision, made after the API settles."

Most people who build agents work in Python or TypeScript. The frameworks they use leave the jail, approvals, sandbox, guardrails, and secret redaction to them, which is what agent86's harness does. A Python or TypeScript program can already reach the harness by running `agent86 run --json` or speaking ACP to a subprocess, but neither lets it hand the agent its own tools or decide approvals in its own code.

The SDK's shape decides most of the work:

- **Callbacks are synchronous and run on the turn's thread.** `approver`, `on_limit`, `on_secret_request`, and `FnTool`'s closure are `Fn(...) + Send + Sync`, called one at a time through the prompt queue (`FnTool` from the tool batch). `Tool::execute` is synchronous too, and runs inside the turn's runtime.
- **A turn is async.** `Agent::run` and `run_with_events` are `async fn`s. `TurnEvent` borrows (`TurnEvent<'_>`), so an event must be copied to cross a language boundary.
- **Cancellation is a `CancelToken`**, checked at the loop's safe points, so a running tool always finishes.
- **Providers are a trait object** (`Arc<dyn Provider>`), whose only required method returns a stream of deltas.
- **Neither default feature is used by `sdk`.** It never reads the keyring, and `Memory::open` uses the hash embedder.

Questions this has to settle:

- One binding at a time, or both together, and which first.
- Where the binding code lives, and how it is versioned and released.
- How each kind of callback crosses the boundary, including the host language's async functions.
- What the host language sees: async or blocking calls, events, errors, cancellation, types.
- Package names, which features are built, and which platforms get binaries.
- What has to change in `sdk` before it is bound.

## Decision

**Ruling:** agent86 publishes Python bindings (PyO3 with maturin, as `agent86-core` on PyPI) in 1.2.0 and TypeScript bindings (napi-rs, as `@maetek/agent86-core` on npm) in 1.2.1. Both are thin wrappers over `agent86_core::sdk` alone, live in this workspace, ship at the workspace's version from the release workflow, and give each language its own async API while the harness keeps its synchronous tools and its safety defaults.

What that means:

- **Python first, then TypeScript, on one design.** Python is where most agent builders work, and PyO3 with maturin is the more mature toolchain. TypeScript follows one patch release later, on the same choices, so its slices are mostly mechanical. Doing both at once would double the surface to review before either has a user.
- **Only `sdk` is bound.** The bindings name no hidden module, so they carry the same stability promise as the Rust API and nothing more. A capability the bindings need and `sdk` lacks is added to `sdk` first, with Rust tests, never reached around.
- **In the workspace, at one version.** Two new crates, `bindings/python` (`agent86-python`) and `bindings/node` (`agent86-node`), `publish = false` on crates.io, each depending on `agent86-core` by path. Their packages take the workspace version, as the npm packages do, so `agent86-core` 1.2.0 on PyPI wraps `agent86-core` 1.2.0 on crates.io. The release workflow and `npm/build.mjs` keep reading the root manifest's first `version` line.
- **Built without the default features.** `sdk` uses neither the OS keyring nor the local embedding model, so the bindings depend on `agent86-core` with `default-features = false`: about 170 crates instead of 370, and smaller binaries. Turning `local-embeddings` on for the bindings is a later decision, if a custom embedder is bound.
- **Tools stay synchronous; the bindings bridge.** The `Tool` trait does not become async for this. A host function is called on the turn's thread, which waits for it, just as a Rust `FnTool` closure holds the turn. A slow host tool holds the turn the same way. An async `Tool` trait remains a possible later change to `sdk`, and would reach the bindings unchanged.
- **Python callbacks.** Every callback (a tool, the approver, `on_limit`, `on_secret_request`) may be a plain function or an `async def`. A plain function is called with the GIL taken on the turn's thread. A coroutine function is scheduled on the event loop that started the turn (`asyncio.run_coroutine_threadsafe`) and the turn's thread waits for its result, so an approver can await a web request or a UI without blocking the loop. A callback that raises becomes a declined approval, a declined limit or secret, or a failed tool result reported to the model, as a Rust closure's `Err` does; the exception's type and message are kept, run through the vault's redaction like any tool output.
- **TypeScript callbacks.** Each callback is a napi thread-safe function, called on the JavaScript thread. It may return a value or a Promise. The turn's thread waits for the result, which works because `run` returns a Promise and never blocks the JavaScript thread. A thrown error or rejected Promise is handled as in Python.
- **Async first in each language.** Python: `await agent.run(goal)`, with `agent.run_sync(goal)` for scripts, and `async for event in agent.stream(goal)`. TypeScript: `await agent.run(goal)` and `for await (const event of agent.stream(goal))`. The turn runs on a Tokio runtime the binding owns, so the host loop keeps running during a turn.
- **Events as plain records with a `kind`.** Each `TurnEvent` is copied into an owned record (`text`, `notice`, `thinking`, `tool_call`, `tool_result`, `usage`, `compacted`) carrying the same fields, with the tool call and result as plain data. An unknown `kind` is documented as possible, so a new event in a later release is additive for the bindings.
- **Cancellation in each language's own way.** Python: cancelling the task awaiting `run` (or leaving `stream` early) sets the agent's cancel token; `agent.cancel()` does the same. TypeScript: `run` and `stream` take an `AbortSignal`. Either way the turn stops at its next safe point, as `Esc` does in the terminal, and the cancelled turn still saves its conversation.
- **Errors as each language's exceptions.** `BuildError` and `RunError` become a small exception hierarchy in Python (`Agent86Error`, then `BuildError` and `RunError`) and `Error` subclasses with a `code` in TypeScript. The message is the Rust error's text, which is already redacted.
- **Built-in providers by their config, not a host-language provider trait.** The bindings construct the four built-in adapters (OpenAI-compatible, Anthropic, Google, Ollama) from keyword arguments or an options object, with the API key passed explicitly, never read from the environment by the binding. Implementing `Provider` in Python or TypeScript means streaming deltas across the boundary, which is left for later, once someone needs it.
- **Owned types only.** `Conversation` crosses as its JSON (`to_json`/`from_json`, the session database's `state_json`) and its accessors return plain records. `Memory`, `McpServer`, `SubAgents`, `Model`, and `Otel` get builder-style wrappers mirroring the Rust ones. Typed signatures ship with each package: a `.pyi` stub file and the `.d.ts` file napi-rs generates.
- **Package names.** PyPI: `agent86-core`, imported as `agent86_core`. The PyPI name `agent86` belongs to the archived Python implementation (1.0.0), whose users run `pip install agent86` for a CLI; publishing a library with a different API under that name would break them. npm: `@maetek/agent86-core`, in the scope the platform packages already use, leaving `agent86` the CLI launcher. Both names were free on 2026-11-19.
- **The same five platforms as the CLI.** Linux x86_64 and ARM64 (glibc 2.35, as the archives), macOS on Apple silicon and Intel, and Windows x86_64. Python wheels are built for the stable ABI (`abi3`) from the minimum Python version up, so one wheel per platform covers every newer Python; an sdist is published too, which builds from source with a Rust toolchain. Node packages follow the CLI's pattern: one package per platform, and a main package that names them as optional dependencies.
- **Released by the tag, like everything else.** New jobs in `release.yml` after `crate`: build wheels with maturin on each platform, publish them to PyPI by trusted publishing (OIDC, no stored token; PyPI accepts a pending publisher for a project that does not exist yet), and install them on three platforms; later the same for npm, with the CLI packages' wait before the main package. Pull requests that touch the bindings get a dry run, as the npm and crate jobs do.
- **Tested without network, in each language.** Each binding has its own test suite (pytest, and Node's test runner) that drives an agent against a local mock server serving the existing recorded fixtures, covering a tool, an approval each way, a callback that raises, an async callback, cancellation, events, and a conversation round trip. CI runs them on Linux, macOS, and Windows on the stable row.
- **Left out:** other languages, WebAssembly, a host-language `Provider` or `Embedder`, project-specialist approvals (as in `sdk`), and a Python or Node port of the CLI.

### What `sdk` needs first

Binding an API copies its shape into two more languages, so a few things are settled in `sdk` before the first slice, each in a 1.1.x patch release with Rust tests:

- **Events that can grow.** `TurnEvent` was a public enum without `#[non_exhaustive]`, so a new variant was a breaking change for a Rust caller that matched every variant. It is now `#[non_exhaustive]` (done 2026-11-19, shipping in 1.1.1; see the settled questions below). That was itself a breaking change for such callers, made while the crate has few users rather than at 2.0. `BuildError`, `RunError`, and `RunResult` got the same marker at the same time, so new error reasons and result fields are additive too.
- **A blocking entry point or a runtime rule.** The bindings drive `Agent::run` from their own Tokio runtime. Its documentation should say which runtime flavors work (a turn that fans out sub-agents needs a multi-threaded runtime, as the tests do), so the bindings and Rust callers follow one rule.
- **Async host callbacks in mind.** The callbacks are `Fn` called on the turn's thread, which is what makes the bridging above possible. That stays a documented guarantee of `sdk`, not an accident of its implementation.

### Questions settled (2026-11-19)

1. **The PyPI name is `agent86-core`**, imported as `agent86_core`. `agent86` stays the archived Python implementation's, so `pip install agent86` keeps giving its users the CLI they have.
2. **Python 3.11 and Node 22 are the minimum versions.** Python 3.10 reached its end of life in October 2026 and Node 20 in April 2026, so these are the oldest lines still maintained. The wheels are built for `abi3` from 3.11.
3. **`TurnEvent` is `#[non_exhaustive]` now**, while the crate is a day old and has few users, rather than at 2.0. It ships in 1.1.1, whose notes say that a Rust `match` on it needs a `_` arm. The binary's own matches got catch-alls in the same change.

Still open, for the first slice's prototype:

4. **Lints.** PyO3's and napi-rs's macros expand to `unsafe` code, which the workspace's `unsafe_code = "forbid"` would reject, and `forbid` cannot be lifted by an `allow`. The binding crates are expected to set their own `unsafe_code = "deny"` with the macros' generated code allowed, keeping hand-written binding code free of `unsafe` and the library at `forbid`; the prototype confirms what the macros need.

### Slices

1.2.0, Python:

1. **The crate and the core loop.** `bindings/python`, the four built-in providers, `Agent` with the builder's plain options, `run` and `run_sync`, function tools (plain and async), the approver, errors, and the tests above. The lints question is answered here.
2. **The rest of `sdk`.** Events and `stream`, cancellation, `Conversation`, `Memory` and sessions, MCP servers, skills, sub-agents, named models and triage, OpenTelemetry, `on_limit`, `on_secret_request`, and `secret`.
3. **Packaging and docs.** The wheel matrix, the sdist, trusted publishing, the install checks, the `.pyi` stubs, a README for PyPI with an example, and the release notes.

1.2.1, TypeScript: the same three slices with napi-rs.

## Consequences

- **Python and TypeScript programs can embed agent86** with their own tools and approvals, written in their own language, and get the same jail, gate, sandbox, guardrails, and redaction as the CLI and the Rust API, with no subprocess.
- **The surface is `sdk`'s, three times over.** A change to `sdk` now also means a change to two bindings, their stubs, and their tests, which is the main reason to settle the points above first and to keep `sdk` small.
- **The release grows.** Two more registries, about twenty more build jobs per release, and more to go wrong on a tag. The jobs follow the npm job's pattern (publish what is missing, skip what is out, so a re-run passes), and PyPI uses trusted publishing, so no new secret is stored for it.
- **A slow host callback holds the turn**, as a slow Rust tool does today. That is documented, and is the price of keeping the harness's tools synchronous.
- **The CLI and its contracts are untouched.** Nothing in `run --json`, the session database, the trace, or the config changes, and the `agent86` npm package stays the CLI.
- **The archived Python implementation is left alone** under its own PyPI name.
