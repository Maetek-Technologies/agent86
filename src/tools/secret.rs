//! `request_secret`, beyond Python: the agent asks the user for a secret through a
//! masked prompt, and gets back only its name (see `crate::vault`).

use super::{failure, success, Arguments, Tool, ToolContext, ToolError};
use crate::types::ToolResult;
use crate::vault::{placeholder, valid_name, NAME_PATTERN};
use serde_json::{json, Map, Value};

pub struct RequestSecretTool;

impl Tool for RequestSecretTool {
    fn name(&self) -> &str {
        "request_secret"
    }

    fn description(&self) -> &str {
        "Ask the user for a secret (an API key, token, or password) through a masked \
         prompt. Never ask for a secret in the chat; call this instead. The value is never \
         shown to you: it is set as the environment variable `name` in run_command and \
         python_exec, and tool output shows ${secret:NAME} where it would appear."
    }

    /// The prompt is the user's consent, so the gate need not ask first; but it
    /// reaches the user, so it never shares a parallel batch.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        json!({
            "properties": {
                "name": {
                    "description": "The environment variable name to set it as, such as STRIPE_API_KEY.",
                    "pattern": NAME_PATTERN,
                    "title": "Name",
                    "type": "string",
                },
                "reason": {
                    "description": "What it is for, shown to the user when they are asked.",
                    "title": "Reason",
                    "type": "string",
                },
            },
            "required": ["name", "reason"],
            "type": "object",
        })
    }

    fn execute(
        &self,
        args: &Map<String, Value>,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut arguments = Arguments::new(args);
        let name = arguments.string("name", None);
        let reason = arguments.string("reason", None);
        arguments.finish()?;
        if !valid_name(&name) {
            return Err(ToolError::InvalidArguments(vec![format!(
                "name: String should match pattern '{NAME_PATTERN}'"
            )]));
        }

        if !ctx.secrets.can_ask() {
            return Ok(failure(
                self.name(),
                format!(
                    "Cannot ask for the secret {name}: nobody is at a terminal to enter it. \
                     Ask the user to run agent86 interactively and enter it with /secret {name}."
                ),
            ));
        }
        if !ctx.secrets.ask(&name, &reason) {
            return Ok(failure(
                self.name(),
                format!("The user declined to enter the secret {name}."),
            ));
        }
        Ok(success(
            self.name(),
            format!(
                "The secret {name} is set. Use it as the environment variable {name} in \
                 run_command and python_exec (${name} in sh, %{name}% in cmd.exe, \
                 os.environ[\"{name}\"] in Python). Its value is hidden from you; tool output \
                 shows {} where it would appear.",
                placeholder(&name)
            ),
        ))
    }
}
