//! Opt-in live smoke test against OpenRouter.
//!
//! Ignored by default and never run in CI. To run it, supply a key through the
//! environment and pick a low-cost model:
//!
//! ```text
//! OPENROUTER_API_KEY=... AGENT86_LIVE_MODEL=openrouter:<model> \
//!     cargo test --test live_openrouter -- --ignored
//! ```

use serde_json::Value;
use std::process::Command;

#[test]
#[ignore = "live network call; needs OPENROUTER_API_KEY and AGENT86_LIVE_MODEL"]
fn a_live_openrouter_turn_returns_text() {
    let (Ok(_), Ok(model)) = (
        std::env::var("OPENROUTER_API_KEY"),
        std::env::var("AGENT86_LIVE_MODEL"),
    ) else {
        panic!("set OPENROUTER_API_KEY and AGENT86_LIVE_MODEL to run this test");
    };

    let output = Command::new(env!("CARGO_BIN_EXE_agent86"))
        .args([
            "--model",
            &model,
            "run",
            "--json",
            "Reply with the single word: pong",
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !payload["output"].as_str().unwrap_or_default().is_empty(),
        "{payload}"
    );
    assert!(payload["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0);
}
