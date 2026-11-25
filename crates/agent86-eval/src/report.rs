//! `summary.md`: pass rates and costs per model and variant, then each task.

use crate::run::Record;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A model and a variant.
type Group = (String, String);

/// Passed and total runs of one task, per [`Group`], keyed by task and category.
type TaskCells = BTreeMap<(String, String), BTreeMap<Group, (usize, usize)>>;

/// The median of `values` (the lower middle for an even count); 0 when empty.
fn median(mut values: Vec<u64>) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) / 2]
}

fn pct(passed: usize, total: usize) -> String {
    if total == 0 {
        "-".into()
    } else {
        format!("{:.0}%", passed as f64 * 100.0 / total as f64)
    }
}

/// The summary of `all`, as Markdown. Runs whose provider could not be reached
/// (`infra`) are counted apart and left out of every score, since they say
/// nothing about the model.
pub fn summary(all: &[Record]) -> String {
    let records: Vec<&Record> = all.iter().filter(|r| !r.infra).collect();
    let mut unreachable: BTreeMap<Group, usize> = BTreeMap::new();
    let mut groups: BTreeMap<Group, Vec<&Record>> = BTreeMap::new();
    for record in all {
        let key = (record.model.clone(), record.variant.clone());
        let runs = groups.entry(key.clone()).or_default();
        if record.infra {
            *unreachable.entry(key).or_default() += 1;
        } else {
            runs.push(record);
        }
    }
    let mut out = String::from("# agent86 eval\n\n");
    let skipped_total: usize = unreachable.values().sum();
    if skipped_total > 0 {
        let _ = writeln!(
            out,
            "{skipped_total} runs could not reach their provider and are left out of the scores; \
             `--resume` runs them again.\n"
        );
    }
    out.push_str("| Model | Variant | Passed | Rate | Median steps | Input tokens | Output tokens | Cost | Mean time | Compactions | Trims | Errors | Unreachable |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (key, runs) in &groups {
        let (model, variant) = key;
        let skipped = unreachable.get(key).copied().unwrap_or(0);
        let passed = runs.iter().filter(|r| r.passed).count();
        let total = runs.len();
        let input: u64 = runs.iter().map(|r| r.input_tokens).sum();
        let output: u64 = runs.iter().map(|r| r.output_tokens).sum();
        let cost: f64 = runs.iter().map(|r| r.cost_usd).sum();
        let time: f64 = runs.iter().map(|r| r.duration_s).sum::<f64>() / total.max(1) as f64;
        let compactions: u64 = runs.iter().map(|r| r.compactions).sum();
        let trims: u64 = runs.iter().map(|r| r.context_trims).sum();
        let errors = runs.iter().filter(|r| r.error.is_some()).count();
        let steps = median(runs.iter().map(|r| r.steps).collect());
        let _ = writeln!(
            out,
            "| {model} | {variant} | {passed}/{total} | {} | {steps} | {input} | {output} | ${cost:.4} | {time:.0}s | {compactions} | {trims} | {errors} | {skipped} |",
            pct(passed, total)
        );
    }

    let mut tasks: TaskCells = BTreeMap::new();
    for record in records.iter().copied() {
        let cell = tasks
            .entry((record.task.clone(), record.category.clone()))
            .or_default()
            .entry((record.model.clone(), record.variant.clone()))
            .or_default();
        cell.1 += 1;
        if record.passed {
            cell.0 += 1;
        }
    }
    out.push_str("\n## By task\n\n| Task | Category |");
    for (model, variant) in groups.keys() {
        let _ = write!(out, " {model} ({variant}) |");
    }
    out.push_str("\n|---|---|");
    for _ in groups.keys() {
        out.push_str("---|");
    }
    out.push('\n');
    for ((task, category), cells) in &tasks {
        let _ = write!(out, "| {task} | {category} |");
        for key in groups.keys() {
            match cells.get(key) {
                Some((passed, total)) => {
                    let _ = write!(out, " {passed}/{total} |");
                }
                None => out.push_str(" - |"),
            }
        }
        out.push('\n');
    }

    let failures: Vec<&Record> = records
        .iter()
        .copied()
        .filter(|r| r.error.is_some())
        .collect();
    if !failures.is_empty() {
        out.push_str("\n## Errors\n\n");
        for record in failures {
            let error = record
                .error
                .as_deref()
                .unwrap_or_default()
                .replace('\n', " ");
            let error: String = error.chars().take(300).collect();
            let _ = writeln!(
                out,
                "- {} / {} / {} #{}: {error}",
                record.task, record.model, record.variant, record.repeat
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(task: &str, variant: &str, passed: bool, steps: u64) -> Record {
        Record {
            task: task.into(),
            category: "bugfix".into(),
            model: "m:x".into(),
            variant: variant.into(),
            repeat: 1,
            passed,
            agent_exit: Some(0),
            timed_out: false,
            steps,
            tool_calls: 0,
            input_tokens: 100,
            output_tokens: 10,
            cost_usd: 0.0,
            compactions: 1,
            context_trims: 0,
            duration_s: 10.0,
            error: (!passed).then(|| "out of steps".to_owned()),
            infra: false,
            check_tail: String::new(),
            final_answer: String::new(),
        }
    }

    #[test]
    fn unreachable_runs_are_counted_apart_and_left_out_of_the_scores() {
        let mut down = record("b", "baseline", false, 0);
        down.infra = true;
        down.error = Some("Cannot reach http://box:8000".into());
        let mut all_down = record("c", "baseline", false, 0);
        all_down.infra = true;
        let text = summary(&[record("a", "baseline", true, 4), down, all_down]);
        assert!(
            text.contains("2 runs could not reach their provider"),
            "{text}"
        );
        assert!(
            text.contains("| m:x | baseline | 1/1 | 100% | 4 |"),
            "{text}"
        );
        // Errors 0 (the unreachable runs are not counted there), Unreachable 2.
        assert!(text.contains("| 0 | 2 |"), "{text}");
        // A task whose every run was unreachable has no cell, and no error line.
        assert!(!text.contains("| b | bugfix |"), "{text}");
        assert!(!text.contains("Cannot reach"), "{text}");
    }

    #[test]
    fn medians_take_the_lower_middle() {
        assert_eq!(median(vec![]), 0);
        assert_eq!(median(vec![5, 1, 3]), 3);
        assert_eq!(median(vec![4, 1, 3, 2]), 2);
    }

    #[test]
    fn the_summary_groups_by_model_and_variant_then_task() {
        let text = summary(&[
            record("a", "baseline", true, 4),
            record("b", "baseline", false, 10),
            record("a", "drop", true, 6),
        ]);
        assert!(
            text.contains(
                "| m:x | baseline | 1/2 | 50% | 4 | 200 | 20 | $0.0000 | 10s | 2 | 0 | 1 | 0 |"
            ),
            "{text}"
        );
        assert!(!text.contains("could not reach"), "{text}");
        assert!(text.contains("| m:x | drop | 1/1 | 100% | 6 |"), "{text}");
        assert!(text.contains("| a | bugfix | 1/1 | 1/1 |"), "{text}");
        assert!(text.contains("| b | bugfix | 0/1 | - |"), "{text}");
        assert!(
            text.contains("- b / m:x / baseline #1: out of steps"),
            "{text}"
        );
    }
}
