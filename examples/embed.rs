//! agent86 embedded in another program: a custom tool, the read-only built-ins,
//! and an approver, against any OpenAI-compatible endpoint.
//!
//! ```bash
//! # A local server (Ollama's OpenAI-compatible endpoint, llama.cpp, vLLM, LM Studio):
//! AGENT86_EXAMPLE_BASE_URL=http://localhost:11434/v1 AGENT86_EXAMPLE_MODEL=llama3.1 \
//!   cargo run --example embed -- "What is in Cargo.toml, and what's the weather in Oslo?"
//! # OpenAI itself: set OPENAI_API_KEY and AGENT86_EXAMPLE_MODEL.
//! ```

use agent86::agent::TurnEvent;
use agent86::provider::openai::{OpenAiConfig, OpenAiProvider};
use agent86::sdk::{Agent, Builtins, FnTool};
use agent86::types::ApprovalMode;
use serde_json::json;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let goal = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let goal = if goal.is_empty() {
        "List the files here, then tell me the weather in Oslo.".to_owned()
    } else {
        goal
    };
    let model = std::env::var("AGENT86_EXAMPLE_MODEL").unwrap_or_else(|_| "gpt-5.4-mini".into());
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let provider = OpenAiProvider::new(
        model,
        OpenAiConfig {
            base_url: std::env::var("AGENT86_EXAMPLE_BASE_URL").ok(),
            require_key: false,
            ..OpenAiConfig::default()
        },
        api_key,
    )?;

    // A tool is a closure and a JSON Schema; the harness validates nothing more,
    // reports errors to the model, and runs read-only tools in parallel.
    let weather = FnTool::new(
        "get_weather",
        "The current weather in a city.",
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
        |args, _ctx| {
            let city = args["city"].as_str().ok_or("city must be a string")?;
            Ok(format!("Sunny and 18C in {city} (a made-up forecast)."))
        },
    );

    let mut agent = Agent::builder()
        .provider(Arc::new(provider))
        .workspace(std::env::current_dir()?)
        .builtins(Builtins::Files)
        .tool(weather)
        .approval(ApprovalMode::Ask)
        // A real program would ask its user; this one refuses every write and says so.
        .approver(|tool, preview| {
            eprintln!("[refused {tool}] {}", preview.summary);
            false
        })
        .max_steps(12)
        .build()?;

    let result = agent
        .run_with_events(&goal, |event| match event {
            TurnEvent::Text(text) => print!("{text}"),
            TurnEvent::ToolCall { text, .. } | TurnEvent::ToolResult { text, .. } => {
                eprintln!("{}", text.trim_end())
            }
            _ => {}
        })
        .await?;
    println!();
    if let Some(turn) = result.turn {
        eprintln!(
            "[{} steps, {} tool calls, {} tokens in, {} out]",
            turn.steps, turn.tool_calls, turn.input_tokens, turn.output_tokens
        );
    }
    Ok(())
}
