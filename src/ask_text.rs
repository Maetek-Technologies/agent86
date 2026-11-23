//! `ask_user` in the plain loop (ADR 0006, slice 3): the agent's questions as
//! text, one at a time, read from the terminal the way the approval prompt
//! reads its answer. Installed by `repl::plain_loop` only when stdin is a
//! terminal, so piped input and `run` never offer the tool.

use crate::ask::{AnswerKind, Form, FormQuestion, Outcome, Reply, Via};
use crate::escape::show_controls;
use serde_json::{Map, Value};
use std::io::{BufRead, Write};

/// What the user types alone to stop answering (the questions are dismissed).
pub const STOP: &str = "/skip";

/// Ask `form`'s questions on `out`, reading the answers from `input`, one line
/// each. End of input, [`STOP`], or `cancelled()` turning true (Ctrl+C in the
/// plain loop cancels the turn) dismisses them. An empty line skips an optional
/// question; a required one, or an answer that does not fit the kind, is asked
/// again with what is wanted.
pub fn ask_in_text(
    form: &Form,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    color: bool,
    cancelled: &dyn Fn() -> bool,
) -> Reply {
    let dismissed = || Reply {
        outcome: Outcome::Dismissed,
        via: Via::Text,
    };
    let bold = |text: &str| {
        if color {
            format!("\x1b[1m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let dim = |text: &str| {
        if color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let heading = match &form.title {
        Some(title) => format!("The agent asks: {}", show_controls(title)),
        None => "The agent asks:".to_owned(),
    };
    let _ = writeln!(out, "\n{}", bold(&heading));
    let _ = writeln!(
        out,
        "{}",
        dim(&format!(
            "One answer per line. Enter alone skips a question marked (optional); {STOP} stops answering."
        ))
    );
    let mut values = Map::new();
    for (number, question) in form.questions.iter().enumerate() {
        let _ = writeln!(
            out,
            "{} {}{}",
            bold(&format!("{}.", number + 1)),
            show_controls(&question.question),
            dim(&hint(question))
        );
        if question.kind.has_options() {
            let listed: Vec<String> = question
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| format!("{}) {}", index + 1, show_controls(option)))
                .collect();
            let _ = writeln!(out, "   {}", listed.join("  "));
        }
        loop {
            let _ = write!(out, "  > ");
            let _ = out.flush();
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => return dismissed(),
                Ok(_) => {}
            }
            if cancelled() {
                return dismissed();
            }
            let line = line.trim();
            if line == STOP {
                return dismissed();
            }
            if line.is_empty() {
                if question.optional {
                    break;
                }
                let _ = writeln!(out, "  {}", dim("An answer is needed here."));
                continue;
            }
            match parse(question, line) {
                Ok(value) => {
                    values.insert(question.id.clone(), value);
                    break;
                }
                Err(wanted) => {
                    let _ = writeln!(out, "  {}", dim(&wanted));
                }
            }
        }
    }
    Reply {
        outcome: Outcome::Answered(values),
        via: Via::Text,
    }
}

/// What follows a question: what kind of answer it takes.
fn hint(question: &FormQuestion) -> String {
    let what = match question.kind {
        AnswerKind::Choice => "a number, or your own words",
        AnswerKind::Multi => "numbers separated by commas, or your own words",
        AnswerKind::Text => "",
        AnswerKind::LongText => "one line",
        AnswerKind::Number => "a number",
        AnswerKind::YesNo => "y or n",
        AnswerKind::Date => "YYYY-MM-DD",
    };
    match (what.is_empty(), question.optional) {
        (true, false) => String::new(),
        (true, true) => " (optional)".to_owned(),
        (false, false) => format!(" ({what})"),
        (false, true) => format!(" ({what}; optional)"),
    }
}

/// One line as the answer to `question`, or what is wanted instead.
fn parse(question: &FormQuestion, line: &str) -> Result<Value, String> {
    let option = |text: &str| -> String {
        text.parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| question.options.get(index))
            .cloned()
            .unwrap_or_else(|| text.to_owned())
    };
    match question.kind {
        AnswerKind::Choice => Ok(Value::String(option(line))),
        AnswerKind::Multi => Ok(Value::Array(
            line.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(option(item)))
                .collect(),
        )),
        AnswerKind::Text | AnswerKind::LongText => Ok(Value::String(line.to_owned())),
        AnswerKind::Number => match line.parse::<f64>() {
            Ok(number) if number.is_finite() => Ok(Value::String(line.to_owned())),
            _ => Err("A number, please (such as 3 or 2.5).".to_owned()),
        },
        AnswerKind::YesNo => match line.to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(Value::Bool(true)),
            "n" | "no" => Ok(Value::Bool(false)),
            _ => Err("y or n, please.".to_owned()),
        },
        AnswerKind::Date => {
            let shaped = line.len() == 10
                && line.char_indices().all(|(index, ch)| {
                    if index == 4 || index == 7 {
                        ch == '-'
                    } else {
                        ch.is_ascii_digit()
                    }
                });
            if shaped {
                Ok(Value::String(line.to_owned()))
            } else {
                Err("A date as YYYY-MM-DD, please.".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form() -> Form {
        Form::parse(
            json!({"title": "Plan", "questions": [
                {"id": "scope", "question": "Which content?", "kind": "choice",
                 "options": ["Everything", "Personal only"]},
                {"id": "goal", "question": "Main goal?", "kind": "multi",
                 "options": ["Less clutter", "Find files"]},
                {"id": "keep", "question": "What must not move?", "kind": "text", "optional": true},
                {"id": "hours", "question": "How many hours?", "kind": "number"},
                {"id": "archive", "question": "Archive backups?", "kind": "yes_no"},
                {"id": "by", "question": "Done by?", "kind": "date"}
            ]})
            .as_object()
            .unwrap(),
        )
        .unwrap()
    }

    fn run(typed: &str) -> (Reply, String) {
        let mut input = std::io::Cursor::new(typed.as_bytes().to_vec());
        let mut out = Vec::new();
        let reply = ask_in_text(&form(), &mut input, &mut out, false, &|| false);
        (reply, String::from_utf8(out).unwrap())
    }

    #[test]
    fn each_question_is_asked_in_turn_and_the_answers_come_back() {
        let (reply, shown) = run("2\n1, archive the photos\n\n2.5\ny\n2026-12-01\n");
        assert_eq!(reply.via, Via::Text);
        let Outcome::Answered(values) = reply.outcome else {
            panic!("answered");
        };
        let accepted = form().accept(&values);
        assert_eq!(
            Value::Object(accepted.answers),
            json!({
                "scope": "Personal only",
                "goal": ["Less clutter", "archive the photos"],
                "keep": null,
                "hours": 2.5,
                "archive": true,
                "by": "2026-12-01"
            })
        );
        assert_eq!(accepted.typed, ["goal"]);
        assert!(shown.starts_with("\nThe agent asks: Plan\n"), "{shown}");
        assert!(shown.contains("1. Which content? (a number, or your own words)\n   1) Everything  2) Personal only\n"), "{shown}");
        assert!(
            shown.contains("3. What must not move? (optional)\n"),
            "{shown}"
        );
        assert!(shown.contains("5. Archive backups? (y or n)\n"), "{shown}");
    }

    #[test]
    fn a_required_or_unfitting_answer_is_asked_for_again() {
        let (reply, shown) = run("\n1\n2\n\nmany\n3\nmaybe\nn\n12/01\n2026-12-01\n");
        assert!(matches!(reply.outcome, Outcome::Answered(_)));
        assert!(shown.contains("An answer is needed here."), "{shown}");
        assert!(
            shown.contains("A number, please (such as 3 or 2.5)."),
            "{shown}"
        );
        assert!(shown.contains("y or n, please."), "{shown}");
        assert!(shown.contains("A date as YYYY-MM-DD, please."), "{shown}");
    }

    #[test]
    fn stop_end_of_input_or_a_cancel_dismisses() {
        assert_eq!(run("1\n/skip\n").0.outcome, Outcome::Dismissed);
        assert_eq!(run("1\n").0.outcome, Outcome::Dismissed);
        let mut input = std::io::Cursor::new(b"1\n".to_vec());
        let reply = ask_in_text(&form(), &mut input, &mut Vec::new(), false, &|| true);
        assert_eq!(reply.outcome, Outcome::Dismissed);
    }

    #[test]
    fn control_characters_from_the_model_are_shown_not_sent() {
        let form = Form::parse(
            json!({"questions": [{"id": "a", "question": "Pick\u{1b}[2J", "kind": "text"}]})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let mut out = Vec::new();
        ask_in_text(
            &form,
            &mut std::io::Cursor::new(b"x\n".to_vec()),
            &mut out,
            false,
            &|| false,
        );
        let shown = String::from_utf8(out).unwrap();
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
    }
}
