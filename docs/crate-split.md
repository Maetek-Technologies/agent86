# Crate split: `agent86-core` and `agent86`

Status: done (2026-11-19), as sketched here with the differences listed under "As built". It was first written as a sketch for review before any code moved; It follows [ADR 0003](adr/0003-embeddable-agent-api.md), which is accepted and says the library moves into its own crate once the API settles. The measurements below were taken on `main` at `be154eb`.

## Goal

- Embedders depend on a library crate that doesn't compile the terminal interface's dependencies (crossterm, clap, rpassword, pulldown-cmark).
- They can also leave out the heavy optional parts: the OS keyring stacks and the candle embedding model.
- The CLI keeps its behaviour, output, release artifacts and CI check names exactly as they are.

## Layout

```
Cargo.toml                 workspace root, and the `agent86` package (binary + its own lib)
src/                       the binary's surfaces: main.rs, ui/, repl.rs, acp/, setup.rs, ...
tests/                     the binary's tests (and, for this PR, most others; see Tests)
crates/agent86-core/
  Cargo.toml               the `agent86-core` package (library only)
  src/                     the agent: harness, providers, tools, memory, MCP, sdk, ...
  tests/                   sdk.rs (moved here, so the API is shown to stand alone)
```

**The binary stays at the root.** That keeps every path the release and npm pipelines use unchanged:
- `target/<target>/release/agent86` is still where the binary lands.
- `release.yml` and `npm/build.mjs` read the version from the root `Cargo.toml` with the first `^version = "…"` line. Putting `[workspace.package] version = "…"` above the members' `version.workspace = true` lines keeps that working.
- The CI jobs keep their names (`ubuntu-latest / 1.94` and so on), so the required checks on `main` need no change in branch protection.

## Where each module goes

**`agent86-core`, about 33,000 lines.** Everything the harness needs to run a turn, with no terminal or CLI in it:

| Group | Modules |
|---|---|
| Vocabulary | `types`, `pyjson`, `difflib`, `escape`, `turn` |
| Models | `provider` (all adapters), `pricing`, `router` |
| Config | `config`, `trust`, `config_writer` (pure TOML editing; `limits` uses it) |
| The loop | `agent`, `limits`, `approval`, `guardrails`, `prompt` |
| Tools | `tools` (files, exec, sandbox, web, search, memory, skills, delegate, secret), `worktree` |
| Sub-agents and skills | `subagent`, `specialty_turn`, `agents_def`, `skills` |
| State | `session`, `memory`, `secrets`, `vault` |
| Telemetry | `observability`, `otel` |
| Integrations | `mcp` |
| The API | `sdk` |

**`agent86`, about 26,000 lines including `main.rs`.** The CLI's surfaces and its config-driven world:

| Module | Why it is binary-side |
|---|---|
| `ui` (11.9k), `repl` (2.6k), `history`, `mentions` | the terminal interfaces |
| `acp` (7.5k), `session_dirs` | the editor server and its `~/.agent86` record |
| `setup` | assembly from config, the home directory, the keyring at a terminal and the terminal prompts (the SDK is the library's own assembly) |
| `keys`, `auth`, `update` | CLI commands and the release check |
| `vault::terminal_prompt` | the hidden terminal secret prompt, moved out of `vault` (see below) |

## Dependencies that point the wrong way

I mapped every `crate::<module>` reference across modules. Only one dependency runs from the library side to the binary side:

- **`vault::terminal_prompt` calls `keys::prompt_hidden`** (rpassword). The only code that installs it is `setup`, which is moving to the binary. So `terminal_prompt` moves with it, next to `keys`, and `vault` keeps only the prompt hook.

Three more look like back-edges but aren't:
- `config`, `skills` and `agents_def` mention `crate::update` or `crate::sdk`, but only in doc comments, which get reworded.
- `limits → config_writer` and `provider::ollama → mcp::format_g` both stay inside the library.

## The public API: two tiers instead of `pub(crate)`

ADR 0003 planned to make everything outside `sdk` private to the crate after the split. The measurements say that won't work:
- The binary side uses **137 distinct library items** (from `types`, `config`, `tools`, `approval`, `agent`, `setup`, `observability` and more).
- It makes **127 calls on `harness.`**, the ACP server and the interface drive the harness directly.

Narrowing that now would mean rebuilding the CLI on top of `sdk` first.

**Proposal:**
- `agent86_core::sdk` is the stable surface, the one semver covers.
- Every other module stays `pub` but `#[doc(hidden)]`, with a crate-level note that it exists for the `agent86` binary and carries no stability promise.
- The binary pins the library exactly (`agent86-core = { path = "crates/agent86-core", version = "=1.0.x" }`) and both share one version from the workspace, so the two always ship together.
- The ADR gets an amendment recording this change from its plan.

**Considered and rejected for now:** making the binary depend only on `sdk`. That would mean adding config-driven assembly, the REPL's command needs and ACP's session control to `sdk`, which is a far larger change than the split. It can be revisited later, one surface at a time.

## Keeping the binary's paths working

The binary's own `lib.rs` re-exports the library modules:

```rust
pub use agent86_core::{agent, approval, config, provider, tools, types, /* ... */};
```

This keeps two things compiling without edits:
- `crate::agent::Harness` and similar paths inside `ui`, `repl` and `acp`.
- `agent86::tools::…` and similar paths in the existing integration tests.

So the change is mostly `git mv` (keeping file history), the two `Cargo.toml` files, and the few edits listed here, rather than rewriting thousands of import lines.

## Dependencies per crate

| Crate | Always | Optional (default on) |
|---|---|---|
| `agent86-core` | tokio, reqwest, futures-util, rusqlite, serde, serde_json, toml, toml_edit, indexmap, regex, ignore, globset, encoding_rs, jiff, url, md5, sha2, sha1_smol, thiserror | `os-keyring`: keyring-core plus the Windows, macOS and Linux store crates. `local-embeddings`: candle-core, candle-nn, tokenizers |
| `agent86` | agent86-core (default features), clap, crossterm, crossterm_winapi (Windows), pulldown-cmark, unicode-width, rpassword, tokio | none |

**What the two features gate:**
- **`os-keyring`:** gates `secrets::OsKeyring`. Its two config-driven users, `provider_for_ref` and `McpManager::start`, fall back to `NoSecretStore` without it. The SDK never reads the keyring either way.
- **`local-embeddings`:** gates `memory::sentence_transformers` and `memory::hub`. Without it, a `sentence-transformers:` embedder falls back to `hash:256` with the existing degradation note. The SDK's `Memory::open` already uses the hash embedder.

An embedder that wants neither would write `agent86-core = { version = "…", default-features = false }`.

## Tests

- **`tests/sdk.rs` moves to `crates/agent86-core/tests/`** and uses `agent86_core::` paths. That shows the published API compiles and works against the library alone.
- **`tests/support/` moves with it.** The root tests that use it point at the new place with `#[path = "../crates/agent86-core/tests/support/mod.rs"]`.
- **Fixtures stay in the root `tests/fixtures/`.** The moved test reaches them through `CARGO_MANIFEST_DIR/../../tests/fixtures`.
- **Every other test stays in the root `tests/` in this PR** and keeps compiling through the re-exports. The roughly 30 library-only suites (provider, tools, memory, MCP, loop) can move to the core crate in a follow-up, so this PR stays mostly file moves.

## CI, release and tooling

- **`ci.yml`:**
  - Test and clippy run with `--workspace`; `cargo fmt --all` already covers every member.
  - The job names stay, so the required checks are unchanged.
  - I'd add one cheap step to an existing job: `cargo check -p agent86-core --no-default-features`, so the feature gates can't silently break.
- **`cargo deny`, Dependabot and the MSRV:**
  - `cargo deny` and Dependabot (`directory: /`) handle workspaces without changes.
  - Both packages take `rust-version`, `edition`, `license` and `repository` from `[workspace.package]`, so the MSRV stays in one place.
- **Release:** `release.yml`'s `cargo build --release --locked --target …` at the root still builds the `agent86` binary, so the archives, provenance and npm packaging are unchanged.
- **Versions:** `CARGO_PKG_VERSION` inside the library (the prompt's environment line, the web user agent, OTel `service.version`, the trace) still reads the shared workspace version.
- **Publishing:** both crates stay `publish = false` in this PR. Publishing is the next step, after checking that the name `agent86-core` is free on crates.io. (Done for 1.1.0: the name was free, the library is published by `release.yml`'s `crate` job on a tag, and the binary stays `publish = false`.)

## Order of commits in the PR

1. **Workspace and moves.** `[workspace]` in the root manifest, `crates/agent86-core/Cargo.toml`, `git mv` of the library modules, and the binary's re-exports. This should build with no behaviour change.
2. **Cut the back-edge.** Move `setup` and `vault::terminal_prompt` to the binary, and reword the doc comments.
3. **Optional features.** Add `os-keyring` and `local-embeddings`, plus the `--no-default-features` check.
4. **Tests.** Move `tests/sdk.rs` and `tests/support/` into the core crate.
5. **CI.** `--workspace` on test and clippy.
6. **Docs.** CLAUDE.md (paths and the two crates), the plan, and an ADR 0003 amendment for the two-tier surface.

Each commit should pass test, fmt and clippy on its own, so a review can follow the steps.

## As built

The four questions below were answered with the recommendations: the binary stays at the root, the API has two tiers, both features are on by default, and the name is `agent86-core` (its availability on crates.io is checked at the publishing step). The work differed from the sketch in these ways:

- **Steps 1 and 2 became one commit.** The library can't compile while `vault` still calls `keys`, so the back-edge was cut in the same commit as the moves.
- **`sdk` re-exports every type its signatures use.** With the other modules hidden, `ApprovalMode`, `TurnEvent`, `ApprovalPreview`, the `Tool` trait and its types, `GuardrailsConfig`, `Embedder`, `CancelToken` and `HarnessError` are re-exported from `sdk`. A new `sdk::providers` holds the `Provider` trait, the request and response types, and the four built-in adapters. Without this, an embedder couldn't build a provider without reaching into a hidden module.
- **A path dependency instead of an exact version pin.** `agent86-core = { path = "crates/agent86-core" }`, with no `version`: the binary is never published and always builds the library from the same checkout. A pinned `=1.0.x` would have put the version in a second place that a release could forget to bump. The version is written once, in `[workspace.package]`, and `docs/RELEASING.md` says so.
- **Two crate-private items became `pub`:** `catalog::get_json` (used by `update`) and `Harness::delegation_offered` (used by `setup`).
- **The fallback note** for a build without `local-embeddings` is Python's own note for a missing package: `sentence-transformers unavailable (ImportError); using the offline hash embedder.`
- **The CI step runs the library's unit tests** with `--no-default-features` (171 pass), rather than only `cargo check`.
- **Measured:** with default features the library's dependency tree is 367 crates; without them it is 172. The same 933 tests pass as before the split.

## Questions for you

1. **Layout:** keep the binary at the root (recommended, since release and npm paths don't change), or put both packages under `crates/`?
2. **API surface:** is the two-tier approach (`sdk` stable, the rest `#[doc(hidden)]` with an exact version pin) acceptable, rather than narrowing the API first?
3. **Features:** should `os-keyring` and `local-embeddings` be on by default (recommended, so `cargo add agent86-core` behaves like the CLI), or off, so embedders get the lightest build unless they ask?
4. **Name:** is `agent86-core` the name you want on crates.io? I'd check it's free before the publishing step.
