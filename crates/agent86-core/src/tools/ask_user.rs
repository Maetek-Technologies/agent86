//! `ask_user`, beyond Python (ADR 0006): the tool's half. It checks the
//! questions and hands them to the main loop, which asks the user through the
//! surface's hook (`crate::ask`) and makes the answers the result.

use super::{failure, success, Tool, ToolContext, ToolError};
use crate::ask::{Form, ASK_KEY, ASK_USER, MAX_OPTIONS, MAX_QUESTIONS};
use crate::types::ToolResult;
use serde_json::{json, Map, Value};

pub struct AskUserTool;

impl Tool for AskUserTool {
    fn name(&self) -> &str {
        ASK_USER
    }

    fn description(&self) -> &str {
        "Ask the user questions and wait for their answers: the default way to get \
         information, a preference, or a decision from them. Send all your questions at once; \
         agent86 shows them as a form (or as text where no form can be drawn) and returns the \
         answers keyed by each question's id. Use choice or multi with options whenever the \
         likely answers are known; \"Other\" is always offered. Never ask the user questions in \
         your reply when you can call this."
    }

    /// It reaches the user and waits, so it never shares a parallel batch.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "title": {
                    "description": "A short heading for the form.",
                    "title": "Title",
                    "type": "string",
                },
                "questions": {
                    "description": format!("1 to {MAX_QUESTIONS} questions, asked together, in this order."),
                    "items": {
                        "properties": {
                            "id": {
                                "description": "The answer's key: letters, digits, and _, starting with a letter or _, unique in the call.",
                                "pattern": "^[A-Za-z_][A-Za-z0-9_]*$",
                                "type": "string",
                            },
                            "question": {
                                "description": "What to ask.",
                                "type": "string",
                            },
                            "kind": {
                                "description": "choice: one of the options; multi: any of them; text: a line; long_text: a paragraph; number; yes_no; date (YYYY-MM-DD).",
                                "enum": ["choice", "multi", "text", "long_text", "number", "yes_no", "date"],
                                "type": "string",
                            },
                            "options": {
                                "description": format!("For choice and multi only: 2 to {MAX_OPTIONS} short answers. \"Other\" is added for you."),
                                "items": {"type": "string"},
                                "type": "array",
                            },
                            "optional": {
                                "default": false,
                                "description": "True if the user may leave it unanswered.",
                                "type": "boolean",
                            },
                        },
                        "required": ["id", "question", "kind"],
                        "type": "object",
                    },
                    "title": "Questions",
                    "type": "array",
                },
            },
            "required": ["questions"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        if !ctx.asker.can_ask() {
            return Ok(failure(
                self.name(),
                "Nobody can answer questions here, so none were asked. Go on with what you \
                 know, or end your turn saying what you need."
                    .into(),
            ));
        }
        let form = match Form::parse(args) {
            Ok(form) => form,
            Err(problems) => {
                return Ok(failure(
                    self.name(),
                    format!(
                        "The questions were not asked. Fix these and call ask_user again:\n{}",
                        problems
                            .iter()
                            .map(|problem| format!("- {problem}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                ))
            }
        };
        // The main loop asks, through the prompt queue and the surface's hook,
        // and replaces this result with the answers.
        let mut result = success(self.name(), String::new());
        result.metadata.insert(
            ASK_KEY.into(),
            serde_json::to_value(&form).expect("a form serializes"),
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::{Outcome, Reply, Via};

    fn context() -> ToolContext {
        let root = std::env::temp_dir().join(format!("agent86-ask-tool-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        ToolContext::new(&root).unwrap()
    }

    fn questions() -> Map<String, Value> {
        json!({"questions": [{"id": "name", "question": "Your name?", "kind": "text"}]})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn with_nobody_to_answer_nothing_is_asked() {
        let result = AskUserTool.execute(&questions(), &context()).unwrap();
        assert!(!result.ok);
        assert!(result
            .error
            .unwrap()
            .starts_with("Nobody can answer questions here"));
    }

    #[test]
    fn checked_questions_go_to_the_loop_in_the_metadata() {
        let ctx = context();
        ctx.asker.set_prompt(Some(Box::new(|_| Reply {
            outcome: Outcome::Dismissed,
            via: Via::Form,
        })));
        let result = AskUserTool.execute(&questions(), &ctx).unwrap();
        assert!(result.ok);
        assert_eq!(
            result.metadata[ASK_KEY],
            json!({"questions": [{"id": "name", "question": "Your name?", "kind": "text"}]})
        );
    }

    #[test]
    fn a_sub_agent_cannot_ask() {
        let ctx = context();
        ctx.asker.set_prompt(Some(Box::new(|_| Reply {
            outcome: Outcome::Dismissed,
            via: Via::Form,
        })));
        assert!(ctx.clone().asker.can_ask(), "clones share the hook");
        assert!(!ctx.fork().asker.can_ask(), "a sub-agent's fork has none");
    }
}
