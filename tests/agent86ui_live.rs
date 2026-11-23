//! The live agent86UI check (ADR 0005, question 4; opt-in, never in CI): how
//! often real models write `render_ui` batches that validate on the first try,
//! and within one correction.
//!
//! Runs only with `AGENT86_TEST_UI_LIVE=1`. The models are
//! `AGENT86_TEST_UI_MODELS`, comma-separated `provider:model` refs, else
//! `ollama:qwen3.8:27b-mlx`. Providers come from the user's own config
//! (`~/.agent86/config.toml`) and keys from their environment variables or the
//! OS keyring, as the CLI resolves them. Each model gets the same five tasks,
//! each `AGENT86_TEST_UI_RUNS` times (default 1), in a throwaway workspace with
//! nothing but `render_ui` offered and every wait dismissed; nothing it draws
//! goes anywhere. `AGENT86_TEST_UI_TASKS` picks another set (ADR 0006):
//! `large`, four forms of seven to nine questions, or `session`, the session
//! that failed, replayed (a prose turn over a home folder with the read-only
//! tools, then a request for a form), which a clean context does not
//! reproduce. The results print as a table:
//!
//! ```text
//! AGENT86_TEST_UI_LIVE=1 cargo test --test agent86ui_live -- --nocapture
//! AGENT86_TEST_UI_LIVE=1 AGENT86_TEST_UI_MODELS=ollama:qwen3.8:27b-mlx,openrouter:openai/gpt-5-mini \
//!     cargo test --test agent86ui_live -- --nocapture
//! ```

use agent86::config::{load_config, ConfigPaths, Overrides};
use agent86::provider::{capabilities::context_window_for, provider_for_ref};
use agent86::sdk::{Agent, ApprovalMode, Builtins, TurnEvent, Ui};
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

/// Larger forms, the size a model asking its clarifying questions writes (ADR
/// 0006): `AGENT86_TEST_UI_TASKS=large`. The first is the session that failed
/// on `claude-haiku-5.5` twelve times in a row (2026-11-23).
const LARGE_TASKS: &[&str] = &[
    "Using a form, ask me these questions about reorganizing my home folder: 1. Scope: the whole home folder, personal content only, or specific folders I list? 2. Main goal (pick any): reduce clutter, find files faster, remove duplicates or old backups, prepare for a move. 3. Project folders (homelab, paperless, sage-*): leave them, or group them under Projects? 4. Loose files at the top level: leave them, group by type, or group by project? 5. Recovery and backup folders: keep, archive together, or delete? 6. Folders that must not move (free text). 7. Output: a plan document, a folder tree, or both?",
    "Using a form, ask me how to set up a new project: its name, the language (Rust, Go, Python, TypeScript), the license (MIT, Apache-2.0, GPL-3.0, none), the features to enable (CI, Docker, docs site, benchmarks; any of them), the minimum supported version, a one-paragraph description, and whether to publish it.",
    "Using a form, ask me about the trip I want planned: the destination, the start and end dates, the number of travellers, the budget (under $1000, $1000 to $3000, over $3000), the kind of stay (hotel, rental, hostel), my interests (food, museums, hiking, nightlife; any of them), dietary restrictions, and whether you should look for flights.",
    "Using a form, take a bug report from me: a title, the severity (low, medium, high, critical), the affected areas (CLI, interface, ACP, providers; any of them), the steps to reproduce, what I expected, what happened instead, my OS (Windows, macOS, Linux), the agent86 version, and whether it happens every time.",
];

/// One task's outcome: how many render_ui calls, and which (if any) was the
/// first to validate.
/// The session that failed (ADR 0006), as it ran: a turn of clarifying
/// questions in prose over a home folder (`home_folder`), with the read-only
/// tools offered, then the request for a form. Turns are split at
/// `TURN_BREAK`, and only the last is measured.
const SESSION_TASKS: &[&str] = &["Let's plan out a better way to organize the content in this folder. Ask me clarifying questions. Do not change anything yet.\n---\nAsk me these questions using a form"];

const TURN_BREAK: &str = "\n---\n";

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
    let set = std::env::var("AGENT86_TEST_UI_TASKS").unwrap_or_default();
    let session = set == "session";
    let tasks = match set.as_str() {
        "large" => LARGE_TASKS,
        "session" => SESSION_TASKS,
        _ => TASKS,
    };
    // Each task this many times (`AGENT86_TEST_UI_RUNS`, default 1): one run
    // of a few tasks is too few to tell a reliable model from a lucky one.
    let runs: usize = std::env::var("AGENT86_TEST_UI_RUNS")
        .ok()
        .and_then(|n| n.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    let tasks: Vec<&str> = tasks
        .iter()
        .flat_map(|task| std::iter::repeat_n(*task, runs))
        .collect();

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
        for task in &tasks {
            let workspace = tempdir(reference);
            if session {
                home_folder(&workspace);
            }
            let mut agent = Agent::builder()
                .provider(Arc::clone(&provider))
                .builtins(if session {
                    Builtins::ReadOnly
                } else {
                    Builtins::None
                })
                .workspace(&workspace)
                .approval(ApprovalMode::Deny)
                .context_window(context_window_for(reference, &config))
                .max_steps(4)
                // Dismisses every wait, as a user closing the form would, so
                // `wait: true` is measured as the interface takes it.
                .ui(Ui::basic().on_wait(|_| None))
                .build()
                .expect("the agent builds");
            let mut outcome = Outcome::default();
            // Every turn but the last runs first, unmeasured.
            let turns: Vec<&str> = task.split(TURN_BREAK).collect();
            let (last, earlier) = turns.split_last().expect("a task has a turn");
            let mut result = Ok(());
            for turn in earlier {
                if let Err(error) = agent.run_with_events(*turn, |_| {}).await {
                    result = Err(error);
                }
            }
            let result = match result {
                Err(error) => Err(error),
                Ok(()) => agent
                    .run_with_events(*last, |event| {
                        if let TurnEvent::ToolResult { call, result, .. } = event {
                            if call.name == "render_ui" {
                                outcome.calls += 1;
                                // A dismissed wait drew (the waiter declined).
                                let drew = result.ok
                                    || result
                                        .error
                                        .as_deref()
                                        .is_some_and(|e| e.contains("did not answer"));
                                if drew && outcome.first_valid.is_none() {
                                    outcome.first_valid = Some(outcome.calls);
                                }
                                // The first failure's first problem (the line after
                                // "Nothing was drawn"), or its only line.
                                if !drew && outcome.error.is_none() {
                                    let text = result.error.as_deref().unwrap_or_default();
                                    let problem = text.lines().nth(1).unwrap_or(text);
                                    outcome.error = Some(problem.chars().take(200).collect());
                                }
                            }
                        }
                    })
                    .await
                    .map(|_| ()),
            };
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
            n = tasks.len()
        ));
        for (task, outcome) in tasks.iter().zip(&outcomes) {
            if outcome.first_valid != Some(1) {
                rows.push(format!(
                    "    {:?}: {} call(s), first valid {:?}; {}",
                    task.chars().take(48).collect::<String>(),
                    outcome.calls,
                    outcome.first_valid,
                    outcome.error.as_deref().unwrap_or("no render_ui call")
                ));
            }
        }
    }
    println!("\nagent86UI live check\n{}", rows.join("\n"));
}

/// A made-up home folder of the session's shape: tool and profile
/// directories, the user's folders, a few projects, recovery folders, and loose
/// files at the top.
fn home_folder(root: &std::path::Path) {
    let dirs = [
        ".azure",
        ".cache",
        ".cargo",
        ".config",
        ".docker",
        ".kube",
        ".local",
        ".npm",
        ".ollama",
        ".rustup",
        ".ssh",
        ".vscode",
        "AppData",
        "Contacts",
        "Desktop",
        "Documents",
        "Downloads",
        "Favorites",
        "Links",
        "Music",
        "OneDrive",
        "Pictures",
        "Saved Games",
        "Searches",
        "Videos",
        "bin",
        "tools",
        "node-22",
        "homelab",
        "paperless",
        "notes",
        "ssl",
        "sage-backups",
        "sage-migration-20260917",
        "sage-recovery-20261001",
        "photos-recovery-20261012",
        "gpu-logs-20260920",
        "gpu-logs-20260923",
        "scratch",
        "old-laptop",
    ];
    let files = [
        ".bashrc",
        ".bash_profile",
        ".gitconfig",
        ".npmrc",
        "NTUSER.DAT",
        "game.md",
        "story.md",
        "index.html",
        "live.pid",
        "notes.txt",
        "todo.md",
        "backup.ps1",
        "cleanup.sh",
        "report-2026-10.csv",
        "report-2026-11.csv",
        "server.log",
        "install.log",
        "keys-README.txt",
        "wishlist.md",
        "draft.docx",
    ];
    for dir in dirs {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for file in files {
        std::fs::write(root.join(file), "x").unwrap();
    }
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
