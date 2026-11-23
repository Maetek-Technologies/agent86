//! The live agent86UI check (ADR 0005, question 4; opt-in, never in CI): how
//! often real models write `render_ui` batches that validate on the first try,
//! and within one correction.
//!
//! Runs only with `AGENT86_TEST_UI_LIVE=1`. The models are
//! `AGENT86_TEST_UI_MODELS`, comma-separated `provider:model` refs, else
//! `ollama:qwen3.8:27b-mlx`. Providers come from the user's own config
//! (`~/.agent86/config.toml`) and keys from their environment variables or the
//! OS keyring, as the CLI resolves them. Each model gets the same five tasks in
//! a throwaway workspace with nothing but `render_ui` offered; nothing it draws
//! goes anywhere. The results print as a table:
//!
//! ```text
//! AGENT86_TEST_UI_LIVE=1 cargo test --test agent86ui_live -- --nocapture
//! AGENT86_TEST_UI_LIVE=1 AGENT86_TEST_UI_MODELS=ollama:qwen3.8:27b-mlx,openrouter:openai/gpt-5-mini \
//!     cargo test --test agent86ui_live -- --nocapture
//! ```

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::provider::{capabilities::context_window_for, provider_for_ref};
use agent86::sdk::{Agent, ApprovalMode, TurnEvent, Ui};
use agent86::types::ModelRef;
use std::sync::Arc;

/// Asks a model would plausibly answer with a surface: a choice, a form with
/// several kinds of input, a confirmation, a slider, and a list template.
const TASKS: &[&str] = &[
    "Using a form, ask me which plan I want: Basic ($5 a month), Pro ($15) or Team ($40).",
    "I want to book a meeting. Show me a form for its title, its date, and whether it repeats weekly.",
    "Ask me to confirm deleting the 12 files in build/, with Delete and Cancel buttons.",
    "Show me a short survey: my name, a rating from 1 to 5 on a slider, and a comment box.",
    "Show these tasks as a list with a Done button on each: write the report, email Sam, book flights.",
];

/// One task's outcome: how many render_ui calls, and which (if any) was the
/// first to validate.
#[derive(Default)]
struct Outcome {
    calls: usize,
    first_valid: Option<usize>,
    error: Option<String>,
}

#[tokio::test(flavor = "multi_thread")]
async fn models_write_valid_render_ui_batches() {
    if std::env::var("AGENT86_TEST_UI_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT86_TEST_UI_LIVE=1 to run the live agent86UI check");
        return;
    }
    let models =
        std::env::var("AGENT86_TEST_UI_MODELS").unwrap_or_else(|_| "ollama:qwen3.8:27b-mlx".into());
    let config = load_config(&ConfigPaths::discover(), &Overrides::default())
        .expect("the user's config loads");

    let mut rows = Vec::new();
    for reference in models.split(',').map(str::trim).filter(|r| !r.is_empty()) {
        let model_ref = ModelRef::parse(reference).expect("a provider:model ref");
        let provider: Arc<dyn agent86::sdk::Provider> = match provider_for_ref(&model_ref, &config)
        {
            Ok(provider) => Arc::from(provider),
            Err(error) => {
                rows.push(format!("{reference}: no provider ({error})"));
                continue;
            }
        };
        let mut outcomes = Vec::new();
        for task in TASKS {
            let workspace = tempdir(reference);
            let mut agent = Agent::builder()
                .provider(Arc::clone(&provider))
                .workspace(&workspace)
                .approval(ApprovalMode::Deny)
                .context_window(context_window_for(reference, &config))
                .max_steps(4)
                .ui(Ui::basic())
                .build()
                .expect("the agent builds");
            let mut outcome = Outcome::default();
            let result = agent
                .run_with_events(*task, |event| {
                    if let TurnEvent::ToolResult { call, result, .. } = event {
                        if call.name == "render_ui" {
                            outcome.calls += 1;
                            if result.ok && outcome.first_valid.is_none() {
                                outcome.first_valid = Some(outcome.calls);
                            }
                            // The first failure's first problem (the line after
                            // "Nothing was drawn"), or its only line.
                            if !result.ok && outcome.error.is_none() {
                                let text = result.error.as_deref().unwrap_or_default();
                                let problem = text.lines().nth(1).unwrap_or(text);
                                outcome.error = Some(problem.to_owned());
                            }
                        }
                    }
                })
                .await;
            if let Err(error) = result {
                outcome.error.get_or_insert(error.to_string());
            }
            let _ = std::fs::remove_dir_all(&workspace);
            outcomes.push(outcome);
        }
        let tried = outcomes.iter().filter(|o| o.calls > 0).count();
        let first = outcomes.iter().filter(|o| o.first_valid == Some(1)).count();
        let within_two = outcomes
            .iter()
            .filter(|o| o.first_valid.is_some_and(|n| n <= 2))
            .count();
        rows.push(format!(
            "{reference}: drew in {tried}/{n}, valid first try {first}/{n}, within one correction {within_two}/{n}",
            n = TASKS.len()
        ));
        for (task, outcome) in TASKS.iter().zip(&outcomes) {
            if outcome.first_valid != Some(1) {
                rows.push(format!(
                    "    {:?}: {} call(s), first valid {:?}; {}",
                    &task[..task.len().min(48)],
                    outcome.calls,
                    outcome.first_valid,
                    outcome.error.as_deref().unwrap_or("no render_ui call")
                ));
            }
        }
    }
    println!("\nagent86UI live check\n{}", rows.join("\n"));
}

fn tempdir(reference: &str) -> std::path::PathBuf {
    let safe: String = reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "agent86-ui-live-{safe}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
