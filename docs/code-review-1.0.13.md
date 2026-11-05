# Code Review: agent86 v1.0.13

Reviewed with the code-reviewer skill. Scope: the core of the project (~45 of ~60
source files — the agent loop, all tools, the sandbox, the secret vault, the MCP
client, all four providers, config/trust, memory, observability, and the CLI).
Not deeply reviewed: the UI layer (`repl.rs`, `ui/*`), `sentence_transformers.rs`,
and the ported utilities (`difflib.rs`, `pyjson.rs`, `mentions.rs`).

## Summary

`agent86` is a command-line agentic harness: a ReAct loop that calls LLM providers
(OpenAI-compatible, Anthropic, Ollama, llama.cpp), runs model-requested tools behind
an approval gate and a sandbox, and keeps long-term memory, a flight recorder, and
MCP client support. It is a deliberate, compatibility-first rewrite of a Python
implementation, and that shows: the code is exceptionally well documented, defensive,
and heavily tested. The security posture is strong for a tool that runs model-chosen
commands. No critical vulnerabilities were found, one latent bug is worth fixing, and
there are a handful of minor issues.

## Critical Issues 🔴

None. The high-risk paths are handled well:

- **SSRF** (`tools/web.rs`): every hop (URL + each redirect) is DNS-resolved and
  every address vetted, then the client is pinned to those addresses via
  `resolve_to_addrs`, which closes the DNS-rebinding TOCTOU. Private/loopback/
  link-local/CGNAT/metadata ranges are all refused.
- **Path traversal** (`tools/mod.rs::resolve_within`): paths are canonicalized
  following symlinks (including dangling links, up to `MAX_LINKS`), then checked
  against the jail. A link pointing outside is refused.
- **Secret leakage** (`vault.rs`, `secrets.rs`): values are redacted from tool
  output, the model's stream (with a `StreamRedactor` that holds back a partial
  tail split across deltas), the trace, and history; `Debug` impls never print
  values; reserved env names (`PATH`, `LD_*`, `PYTHON*`, …) can't be used as
  secret names.
- **Config-driven RCE** (`trust.rs`): a project's `.agent86/config.toml` is not
  trusted until the user approves it; only a `TRUST_FREE` allowlist counts
  otherwise, and trust is remembered by SHA-256 so any change re-triggers the
  prompt.

## Major Issues 🟠

### 1. Sub-agent routing is inconsistent — a latent bug (`subagent.rs`)

```rust
let provider = self.active_provider();          // routed provider (or session's)
...
let completion = self
    .provider                                    // always the SESSION provider
    .complete(CompletionRequest {
        model: provider.model(),                 // routed model name
        ...
```

The request is sent to `self.provider` (the session provider), but
`CompletionRequest.model` carries the *routed* model name. Every provider ignores
`request.model` and uses its own `self.model` in the payload (`openai.rs`,
`anthropic.rs`, and `ollama.rs` all emit `"model": self.model`). So today the
sub-agent silently uses the session model even when triage routing sent the turn
to a different provider — the `provider` local is dead except for a field that is
discarded.

This is documented as matching Python, but the code is misleading and is a trap:
the moment any provider is made to honor `request.model`, the sub-agent will send
a routed model name to the session provider's endpoint and fail. Either send the
request to `provider.complete(...)` (use the routed model, as the comment
implies) or drop `let provider = self.active_provider()` and the `model` field.
Related: `CompletionRequest.model` is vestigial across the codebase — consider
removing it or making it authoritative.

### 2. `fit` is O(n²) per step (`memory/working.rs::fit`)

```rust
while start > 0 {
    let trial = &messages[start - 1..];
    if start < messages.len() && count_tokens(trial) > budget { break; }
    start -= 1;
}
```

Each iteration re-counts tokens over the whole suffix, so a long conversation is
re-scanned quadratically, and this runs on *every* step (plus `fits` and
`compaction_cut` in `compact_if_needed`). It's bounded by the context window so
it won't blow up, but for long sessions it's the dominant per-step cost. An
incremental count (track the running total as `start` moves) makes it O(n).

## Minor Issues 🟡

1. **`web_fetch` builds a new `reqwest::Client` per redirect hop**
   (`tools/web.rs::fetch`). Each hop gets a fresh connection pool/TLS handshake.
   Hoist one client out of the loop and reuse it.
2. **`SandboxPolicy.network` is always `true`** (`tools/mod.rs`).
   `SandboxPolicy::new` sets it and nothing ever sets it to `false`, so
   `require_network()` is a no-op and the field is dead. Wire it to a config key
   or remove it.
3. **`count_spec_tokens` re-serializes the entire tool catalogue every step**
   (`memory/working.rs`, called from `Harness::context_budget`). With many MCP
   tools this is a non-trivial JSON dump per step; cache it and invalidate when
   the registry changes.
4. **Docker executor passes secrets in the `docker run` client's environment**
   (`tools/sandbox.rs::DockerExecutor::run`). The values are correct on the wire
   to the daemon, but they sit in the client process's `environ` (readable via
   `/proc/<pid>/environ` by other local users). The subprocess backend has the
   same property (that's its point), so this is consistent, but worth a comment
   that the guarantee is "not in the process list," not "not in `/proc`."
5. **`html_to_text`/`drop_blocks` is still potentially quadratic**
   (`tools/web.rs`). The `unclosed` cache fixes the common case (repeated
   unclosed `<form>`), but `find_ignore_ascii_case` is a linear scan per tag and
   `unclosed.contains` is O(n); a hostile 2 MB page of nested tags can still
   degrade. Bounded by `MAX_BODY_BYTES`, so low risk.
6. **`run_subprocess` polls with a 10 ms sleep** (`tools/sandbox.rs`). Fine, but
   a blocking read with a deadline (or `wait` + a timer) would avoid the
   busy-poll. Cosmetic.

## Recommendations 💡

1. **Resolve the sub-agent/provider-model inconsistency** (Major #1) — the
   highest-value fix. Decide whether sub-agents follow routing, make the code
   say that, and either honor or delete `CompletionRequest.model`.
2. **Make `fit` incremental** (Major #2).
3. **Reuse one `reqwest::Client` across `web_fetch` redirects.**
4. **Wire or remove `SandboxPolicy.network`.**
5. **Cache the tool-catalogue token count** keyed on registry identity.
6. `unsafe_code = "forbid"` is already in place (`Cargo.toml`) — good; keep it.

## Positive Aspects ✅

- **Documentation is outstanding.** Every module opens with a doc comment
  explaining what it ports from Python and, crucially, calling out each
  deliberate "Beyond Python" change and *why*. This makes a 60-file codebase
  genuinely navigable.
- **Security is thought through, not bolted on**: per-hop SSRF vetting with
  address pinning, symlink-following path jail, a secret vault with streaming
  redaction, an approval gate that always asks about agent86's own config/skills
  even in `auto` mode, project-config trust, and a `--yes` warning that names
  the actual risk.
- **Defensive, degrading error handling**: memory, MCP, and observability
  failures degrade to "no feature," never a crashed turn; the recorder and
  tracer are best-effort by design.
- **Atomic writes everywhere** (temp file + rename) for config, files, the trust
  store, the history, and downloads — a crash mid-write never truncates.
- **Strong test coverage** with unit tests in nearly every module, plus
  compatibility tests against fixtures extracted from the Python source
  (guardrail patterns, the system prompt, pricing arithmetic, keyring layout).
- **Clean architecture**: a provider-neutral `Provider` trait with one `stream`
  implementation from which `complete` is derived; a `Tool` trait with
  `side_effecting`/`parallel_safe`/`expands_secrets`/`guarded_target` hooks; a
  `SandboxPolicy` that centralizes the jail.
- **Good hygiene**: `unsafe_code = "forbid"`, clippy `all = "warn"`, pinned
  `candle` with a comment explaining the MSRV, and a `deny.toml` for dependency
  vetting.

## Scope and limits

Reviewed with the code-reviewer skill. Scope: the core of the project (~45 of
~60 source files — the agent loop, all tools, the sandbox, the secret vault, the
MCP client, all four providers, config/trust, memory, observability, and the
CLI). Not deeply reviewed: the UI layer (`repl.rs`, `ui/*`),
`sentence_transformers.rs`, and the ported utilities (`difflib.rs`, `pyjson.rs`,
`mentions.rs`).
