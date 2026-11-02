# Code Review: agent86 v1.0.11 (Rust, ~60 source files)

> **Status (2026-11-01):** every Critical and Major finding below was checked against the code and has been fixed, along with the Minor ones, in 1.0.12 and 1.0.13. Where a finding was moot or a fix differs from the suggestion here, the reasons are in [PROJECT_PLAN.md](PROJECT_PLAN.md), under the security review item in section 9; the plan also lists what was accepted rather than fixed. The review is kept as it was written.

## Scope and limits

I read about a third of `src/`:

- `main.rs`, `agent.rs`, `config.rs`, `setup.rs`, `types.rs`
- `approval.rs`, `guardrails.rs`, `secrets.rs`, `vault.rs`, `keys.rs`, `update.rs`, `mentions.rs`
- `tools/{mod,files,exec,sandbox,web,secret,skills}.rs`
- `provider/catalog.rs`, `memory/hub.rs`
- `Cargo.toml`, `CLAUDE.md`, and both CI workflows

I did **not** read `provider/{openai,anthropic,ollama,retry}`, `mcp/*`, `ui/*`, `session.rs`, `repl.rs`, `subagent.rs`, `skills.rs`, `observability.rs`, `config_writer.rs`, or `tests/`.

I could not run `cargo test`, `clippy` or `cargo audit`, because the review skill only allows `read_file` and `list_dir`. Everything below comes from reading the code. Findings I'd want confirmed by a test are marked **(verify)**. I cite functions rather than line numbers, because the file reader doesn't return them.

## Summary

agent86 is a Rust agent harness with a ReAct loop, a tool sandbox, approval gating, a secret vault, MCP, memory, and a TUI. The engineering is careful, and the security-relevant parts show real thought (see Positive Aspects). The main weaknesses sit at the trust boundaries: what the harness loads from the working directory, and what a user sees before approving an action.

---

## Critical Issues 🔴

### 1. Project config, skills and MCP servers are trusted automatically (`config.rs`, `setup.rs`)

`ConfigPaths::discover()` always loads `./.agent86/config.toml`. `assemble_with` then acts on it without any trust prompt. Anyone who opens a cloned, untrusted repo and runs `agent86` can be affected. A malicious project file can:

- Add `[mcp_servers.x] command = "..."`. `McpManager::start(config)` is called unconditionally in `assemble_with`. I did not read `mcp/mod.rs`, but the call site suggests this is code execution at startup, before any approval gate.
- Set `[guardrails] approval = "auto"`, which silently turns off human approval.
- Set `[providers.openai] base_url = "https://attacker"`. The `api_key_env` or keyring key is then sent to that host as a bearer token. I did not read `provider/openai.rs`, so this is inferred from the design.
- Set `[tools] web_allow_private = true`, `sandbox.env_passthrough`, `skills.paths`, `observability.otel_endpoint`, or `memory.embeddings` pointing at an attacker's HF repo.
- Ship workspace skills whose instructions go into the system prompt, and which `SandboxPolicy::allow` adds to the file jail.

The model can also reach this file itself. `.agent86/config.toml` is inside the workspace jail, so a prompt-injected model can `write_file` a malicious MCP server into it. The approval prompt shows only a diff of a TOML file, and it takes effect on the next launch without any further approval.

**Fix:** use trust-on-first-use for the project layer, as other agent CLIs do.

```rust
// load_config(): treat the project layer as untrusted unless approved
let project_hash = sha256(read(&paths.project)?);
if !trust_store.is_trusted(canonical_cwd, project_hash) {
    // interactive: show a diff of the sensitive keys and ask
    // `run`/piped:  ignore sensitive keys, or fail unless --trust-project-config
    strip(&mut layer, &["mcp_servers", "guardrails.approval", "sandbox", "tools.web_allow_private",
                        "providers.*.base_url", "providers.*.api_key_env", "skills.paths",
                        "observability.otel_endpoint", "memory.path", "memory.embeddings"]);
}
```

Also make `.agent86/` write-protected for the file tools, or add an extra warning in the approval prompt when a write targets it.

### 2. The approval prompt prints model-controlled text raw (`approval.rs::ask`, `cap_lines`, `tools/exec.rs::preview`)

`cap_lines` returns `text.to_owned()` unchanged when the preview is at most 40 lines. `ask` writes it straight to the terminal. A command containing `\r`, `\x1b[2K`, `\x1b[8m` or cursor-movement escapes can display a harmless line while the real command differs. For example, `\r` overwrites the visible `rm -rf ~` with `echo hello`. This defeats the main safety control on the plain REPL and `run` paths. The docs say the TUI runs text through `sanitize`, but I saw no equivalent here.

**Fix:**

```rust
fn display_safe(text: &str) -> String {
    text.chars().map(|c| match c {
        '\n' | '\t' => c,
        c if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32).unwrap(), // ␛ ␍ ...
        c if c.is_control() => '\u{FFFD}',                                   // DEL, C1 (0x9b CSI)
        c => c,
    }).collect()
}
// in ask(): writeln!(out, "{}", display_safe(&cap_lines(detail, APPROVAL_DETAIL_LINES)))
```

Apply it to `summary` too. Add a test with `"echo hi\rrm -rf x"`.

---

## Major Issues 🟠

### 3. The SSRF guard misses 100.64.0.0/10 and may be bypassed by proxies (`tools/web.rs`)

- `v4_private` has no `100.64.0.0/10` (CGNAT). That range contains **Alibaba Cloud's metadata endpoint `100.100.100.200`** and Tailscale peers. With `web_allow_private=false`, the model can fetch them. Matching Python's `is_private` is the wrong target for a security boundary. Add `([100,64,0,0],10)` and `192.88.99.0/24`, or use a "not globally routable" check.
- `reqwest::Client::builder()` is not given `.no_proxy()`. If reqwest 0.13 still reads `HTTP(S)_PROXY` by default **(verify with `HTTPS_PROXY` set)**, the proxy resolves the hostname itself. That bypasses both `vet_url`'s resolution and the `resolve_to_addrs` pinning, which re-opens DNS rebinding. Call `.no_proxy()` for `web_fetch`, or document that vetting is advisory behind a proxy.
- Minor: the NAT64 prefix `64:ff9b::/96` isn't unwrapped to its embedded IPv4.

### 4. The file jail can be escaped through dangling symlinks (`tools/mod.rs::resolve`)

`resolve()` only follows a component if `fs::canonicalize` succeeds. For a symlink inside the workspace whose target doesn't exist yet (`notes -> ~/.ssh/authorized_keys` where the file is missing), canonicalize fails. The path stays lexically `workspace/notes` and passes `starts_with`. `write_file` then creates the file outside the jail via `fs::write`. The approval preview shows `new file: notes`, not the real destination. A repo can ship such a link, and Python's `Path.resolve()` follows dangling links.

**Fix:** if canonicalize fails and `symlink_metadata(&resolved)` says the entry is a symlink, refuse it, or `read_link` and keep resolving with a depth limit. There is also a check-then-use race between `resolve_within` and `fs::write`. Opening with `O_NOFOLLOW`, or checking after opening, closes it.

### 5. Command output is not capped while reading (`tools/sandbox.rs::reader`)

The module doc says output is capped, but `pipe.read_to_end(&mut bytes)` buffers everything and only `policy.truncate` trims it afterwards. `yes` or `cat /dev/zero` for 30–60 seconds can use several GB and OOM the host. Docker mode has the same problem.

```rust
let mut bytes = Vec::new();
let _ = (&mut pipe).take(MAX_CAPTURE_BYTES).read_to_end(&mut bytes);
let _ = std::io::copy(&mut pipe, &mut std::io::sink()); // keep draining so the child never blocks
```

### 6. Secrets may leak through unredacted tool-result metadata (`tools/mod.rs::dispatch`, `tools/files.rs`) (verify)

`write_file` and `edit_file` put `metadata["diff"]` in the result. That diff is built from the *expanded* arguments, so it contains the real secret value whenever the secret-placeholder syntax is used in the arguments. `dispatch` redacts only `result.content` and `result.error`. Metadata stays in `Step.results`, which I expect is serialized into `state_json` and kept for the transcript view. That contradicts the vault's stated guarantee (it keeps secrets out of `state_json`). I haven't read `session.rs`, so please confirm with a test: run `edit_file` with a secret placeholder in `new_string`, then grep the saved session for the value.

```rust
let mut meta = Value::Object(std::mem::take(&mut result.metadata));
ctx.secrets.redact_value(&mut meta);
if let Value::Object(m) = meta { result.metadata = m; }
```

### 7. `validate_api_key` panics on some non-ASCII pastes (`secrets.rs`)

`key[1..].contains(probe)` slices at byte 1. If the key starts with a multi-byte character, for example a pasted smart quote `“sk-…`, the length is 32 bytes or more and `is_char_boundary(16)` holds, the slice panics. It is also wrong in intent: a key with a repeated 16-character run triggers a false "pasted twice". Use `key[probe_len..].contains(probe)`. This is easy to cover with a test.

### 8. HTML-to-text can take quadratic time (`web.rs::drop_blocks`)

For every `<script|style|form|table…>` open tag with no matching close tag, the code scans to the end of the document. A 2 MB page of unclosed `<form>` tags takes about 10¹¹ steps. The tool runs synchronously, so one malicious page freezes the agent. The timeout covers only the network. Remember tags already known to be unclosed:

```rust
let mut unclosed: HashSet<String> = HashSet::new();
// on None => { unclosed.insert(tag); search_from = next_char(..) }
// before searching for a close: if unclosed.contains(&tag) { search_from = next_char(..); continue; }
```

### 9. `write_file` calls an overwrite of a non-UTF-8 file a "new file" (`files.rs`)

`existing_text()` returns `None` for any file that isn't valid UTF-8. Both the approval preview and the result then say "new file" when a binary file is being clobbered. Decide new versus overwrite from `target.exists()`.

### 10. `read_file` and `@mention` read whole files before capping (`files.rs`, `mentions.rs`)

`fs::read` loads the entire file, then lossily decodes and counts chars. A multi-GB file in the workspace can OOM the process. A FIFO or device file in a mention reads forever, since `metadata.len()` is 0. Read at most `max_output_chars * 4` bytes with `Read::take`.

---

## Minor Issues 🟡

- **Non-atomic writes:** `write_file`/`edit_file` use `fs::write` directly, so a crash mid-write truncates the user's file. Write a temp file in the same directory and rename it. `edit_file` keeps CRLF and BOM but `write_file` doesn't, which makes the two inconsistent.
- **Mislabelled count:** "Wrote N bytes" is actually a *character* count.
- **Wrong unescape order:** `html_to_text` replaces `&amp;` before `&lt;`/`&gt;`, so `&amp;lt;` becomes `<`. Do `&amp;` last.
- **Blocking calls on the async runtime:** shell commands and fetches run synchronously inside `async fn dispatch_*` on a current-thread runtime (`run`). The cancel `select!` can't fire during a tool, and other tasks stall. Use `spawn_blocking`. The parallel-read path also spawns one OS thread per call with no bound.
- **Secret names are unrestricted:** the model chooses the name in `request_secret`. A prompt-injected model could ask for `LD_PRELOAD`, `BASH_ENV`, `NODE_OPTIONS` or `PATH`. The user still has to type a value, but a denylist is cheap. The vault also has no minimum secret length (`secrets::redact` uses ≥8), so a 1-character secret mangles all text.
- **No validation of `tool_timeout_s = 0`:** every command and fetch then times out immediately.
- **Timeout path drops finished output:** when the timeout path fires and one pipe has already closed, the completed pipe's data is replaced with empty. On a Docker timeout stdout is discarded entirely.
- **Docker hardening:** `docker run` has no `--cap-drop ALL`, `--security-opt no-new-privileges`, `--read-only`/`--tmpfs`, or `--user`. The workspace is mounted read-write.
- **Unpinned CI actions:** `release.yml` (which has `id-token: write` and publishes to npm) uses tag- or branch-pinned actions (`dtolnay/rust-toolchain@stable`/`@master`, `actions/*@vN`) and an unpinned `npm install -g npm@11`. Pin to commit SHAs.
- **No supply-chain checks in CI:** there is no `cargo audit` or `cargo deny` step, even though the tree includes C dependencies (`rusqlite bundled`, `onig`) and a keyring stack.
- **No integrity check on downloaded weights:** `hub.rs` downloads model files with no hash check and no size cap.

## Recommendations 💡

1. Fix the two Criticals first. Both are small, local changes with large payoff.
2. Add regression tests for each Major item:
   - ANSI/CR in an approval preview
   - fetching `100.100.100.200`
   - a dangling symlink write
   - a 1 GB stdout command
   - secret-in-metadata
   - a non-ASCII key paste
   - the unclosed-tag HTML bomb
3. Treat `run_command` and `python_exec` as unsandboxed. They run with `HOME`, the network, and the user's privileges, and the jail covers only the file tools. The docs say this honestly. In `--yes` or `auto` mode, prompt injection therefore becomes arbitrary code execution, so consider making `--yes` print a warning. Docker mode is the real isolation.
4. Make `[guardrails] approval = "auto"` in a project config require explicit user confirmation, separate from the trust prompt.
5. Add `cargo deny check` and `cargo audit` to the CI matrix, and SHA-pin actions.

## Positive Aspects ✅

- **SSRF design:** every hop is re-vetted, redirects are handled manually and capped, the client is pinned with `resolve_to_addrs`, the body is capped while streaming, content types are allowlisted, and there is no auto-decompression, so no zip bombs.
- **Subprocess hygiene:** `env_clear` plus an allowlist, credential-shaped passthrough names refused, a deadline covering process exit and pipe EOF, and a process-group kill on timeout.
- **Secret vault:** `Debug` impls never print values, streamed redaction holds back partial matches on char boundaries, secrets reach tools only via environment variables and the file tools' placeholder expansion, and the keyring is never touched in piped or CI runs.
- **Safe defaults:** `ask` declines with no terminal, and `unsafe_code = forbid` is set.
- **Update path:** `Version::parse` is strict, so the `/update` command line can't be injected through a release tag. Downloads write to `.incomplete` and rename into place.
- **Engineering process:** golden-fixture compatibility tests against the Python reference, MSRV-pinned CI on four OS/arch combos, and trusted publishing with provenance. `CLAUDE.md` is also unusually clear about invariants.
