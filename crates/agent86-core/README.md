# agent86-core

The agent behind [agent86](https://github.com/Maetek-Technologies/agent86), as a library: an agent runtime you can point at a real machine and embed in your own Rust program.

`agent86_core::sdk` assembles the same harness the agent86 command line, terminal interface, and editor server run, from parts you hand it. It reads no config file, environment variable, home directory, keyring, or terminal. What it keeps from the CLI, unchanged:

- **The workspace jail.** Every path a tool touches is resolved inside the workspace you name, symlinks included.
- **The approval gate.** Side-effecting calls go to your `approver` closure, with the same preview (arguments, diff) the CLI shows. With no approver, they are declined.
- **The sandbox.** Commands run with a scrubbed environment, a deadline, and a process-tree kill. `web_fetch` is guarded against SSRF.
- **The guardrails and the secret vault.** Secrets reach tools as environment variables, and the model only ever sees `${secret:NAME}`.
- **The loop.** The ReAct loop and its circuit breaker, parallel read-only calls, context fitting and compaction, and a stream of turn events.

On request it adds MCP servers, long-term memory and resumable sessions, skills, sub-agents (built-in and your own specialists, in parallel or in git worktrees), named models with triage routing, OpenTelemetry spans, and callbacks for the questions a turn asks mid-way (raising a limit, a secret the model needs). Providers for OpenAI-compatible servers, Anthropic, Google, and Ollama are included, and `sdk::providers::Provider` is a trait you can implement.

## Example

```rust,no_run
use agent86_core::sdk::providers::{OpenAiConfig, OpenAiProvider};
use agent86_core::sdk::{Agent, ApprovalMode, Builtins, FnTool};
use serde_json::json;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(
        "gpt-5.4-mini",
        OpenAiConfig::default(),
        std::env::var("OPENAI_API_KEY").ok(),
    )?;
    let weather = FnTool::new(
        "get_weather",
        "The current weather in a city.",
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        |args, _ctx| Ok(format!("Sunny in {}", args["city"].as_str().unwrap_or("?"))),
    );
    let mut agent = Agent::builder()
        .provider(Arc::new(provider))
        .workspace("./project")
        .builtins(Builtins::Files)
        .tool(weather)
        .approval(ApprovalMode::Ask)
        .approver(|tool, _preview| tool != "write_file")
        .build()?;
    let reply = agent.run("What's in Cargo.toml, and the weather in Oslo?").await?;
    println!("{}", reply.answer);
    Ok(())
}
```

A fuller example, with memory and resumable sessions, is [`examples/embed.rs`](https://github.com/Maetek-Technologies/agent86/blob/main/crates/agent86-core/examples/embed.rs).

## Stability

`agent86_core::sdk` is the public API, and the only part covered by semantic versioning. The crate's other modules are public because the `agent86` binary is built on them; they are hidden from the documentation and may change in any release.

## Features

Both are on by default, so the library behaves as the CLI does. Turn them off with `default-features = false` for a lighter build (about 170 dependencies instead of 370).

- `os-keyring`: the OS keyring (Windows Credential Manager, the macOS Keychain, the Secret Service), used by the CLI's config-driven assembly. The `sdk` never reads the keyring either way.
- `local-embeddings`: the local sentence-transformers embedder for memory, on candle. Without it a `sentence-transformers:` embedder falls back to the offline hash embedder, which is what `sdk::Memory::open` uses anyway.

The minimum supported Rust version is 1.94.

## License

MIT
