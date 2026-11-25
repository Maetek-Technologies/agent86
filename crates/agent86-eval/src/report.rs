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

/// The summary of `records`, as Markdown.
pub fn summary(records: &[Record]) -> String {
    let mut groups: BTreeMap<Group, Vec<&Record>> = BTreeMap::new();
    for record in records {
        groups
            .entry((record.model.clone(), record.variant.clone()))
            .or_default()
            .push(record);
    }
    let mut out = String::from("# agent86 eval\n\n");
    out.push_str("| Model | Variant | Passed | Rate | Median steps | Input tokens | Output tokens | Cost | Mean time | Compactions | Trims | Errors |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for ((model, variant), runs) in &groups {
        let passed = runs.iter().filter(|r| r.passed).count();
        let total = runs.len();
        let input: u64 = runs.iter().map(|r| r.input_tokens).sum();
        let output: u64 = runs.iter().map(|r| r.output_tokens).sum();
        let cost: f64 = runs.iter().map(|r| r.cost_usd).sum();
        let time: f64 = runs.iter().map(|r| r.duration_s).sum::<f64>() / total as f64;
        let compactions: u64 = runs.iter().map(|r| r.compactions).sum();
        let trims: u64 = runs.iter().map(|r| r.context_trims).sum();
        let errors = runs.iter().filter(|r| r.error.is_some()).count();
        let steps = median(runs.iter().map(|r| r.steps).collect());
        let _ = writeln!(
            out,
            "| {model} | {variant} | {passed}/{total} | {} | {steps} | {input} | {output} | ${cost:.4} | {time:.0}s | {compactions} | {trims} | {errors} |",
            pct(passed, total)
        );
    }

    let mut tasks: TaskCells = BTreeMap::new();
    for record in records {
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

    let failures: Vec<&Record> = records.iter().filter(|r| r.error.is_some()).collect();
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
            check_tail: String::new(),
            final_answer: String::new(),
        }
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
                "| m:x | baseline | 1/2 | 50% | 4 | 200 | 20 | $0.0000 | 10s | 2 | 0 | 1 |"
            ),
            "{text}"
        );
        assert!(text.contains("| m:x | drop | 1/1 | 100% | 6 |"), "{text}");
        assert!(text.contains("| a | bugfix | 1/1 | 1/1 |"), "{text}");
        assert!(text.contains("| b | bugfix | 0/1 | - |"), "{text}");
        assert!(
            text.contains("- b / m:x / baseline #1: out of steps"),
            "{text}"
        );
    }
}
