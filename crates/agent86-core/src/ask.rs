//! `ask_user`, beyond Python (ADR 0006): the model asks the user its questions
//! as a short, flat list, and agent86 asks them, as a form it builds itself
//! (so the form is always valid), through an ACP extension, or as text.
//!
//! The tool (`tools::ask_user`) checks the arguments ([`Form::parse`]) and hands
//! the [`Form`] to the main loop, which asks through the [`Asker`]'s hook, one
//! question at a time with approvals and secrets (`approval::PromptQueue`), and
//! makes the [`Reply`] the call's result ([`Accepted::text`]). Each surface
//! installs its own hook, as it installs its approval and secret prompts, and
//! the tool is offered only where one is: never under `run`, in CI, or to a
//! sub-agent.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex, PoisonError};

/// The tool's name.
pub const ASK_USER: &str = "ask_user";

/// The `ToolResult::metadata` key under which the tool hands the loop the
/// checked [`Form`] to ask. Taken out before the result reaches the session.
pub const ASK_KEY: &str = "agent86_ask";

/// At most this many questions in one call.
pub const MAX_QUESTIONS: usize = 10;
/// A choice question's options, at least and at most.
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 12;
/// The longest question text, title, and option, in characters.
pub const MAX_QUESTION_CHARS: usize = 300;
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_OPTION_CHARS: usize = 120;

/// What kind of answer a question takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerKind {
    /// Exactly one of the options.
    Choice,
    /// Any number of the options.
    Multi,
    /// A line of text.
    Text,
    /// A paragraph.
    LongText,
    /// A number.
    Number,
    /// Yes or no.
    YesNo,
    /// A date, `YYYY-MM-DD`.
    Date,
}

impl AnswerKind {
    pub const ALL: [AnswerKind; 7] = [
        AnswerKind::Choice,
        AnswerKind::Multi,
        AnswerKind::Text,
        AnswerKind::LongText,
        AnswerKind::Number,
        AnswerKind::YesNo,
        AnswerKind::Date,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AnswerKind::Choice => "choice",
            AnswerKind::Multi => "multi",
            AnswerKind::Text => "text",
            AnswerKind::LongText => "long_text",
            AnswerKind::Number => "number",
            AnswerKind::YesNo => "yes_no",
            AnswerKind::Date => "date",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// Whether the question offers options (and so "Other").
    pub fn has_options(self) -> bool {
        matches!(self, AnswerKind::Choice | AnswerKind::Multi)
    }
}

/// One question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormQuestion {
    /// An identifier, unique in its form; the answer's key.
    pub id: String,
    /// What is asked.
    pub question: String,
    pub kind: AnswerKind,
    /// For `choice` and `multi`: each option is its own label and value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Whether the form may be sent without an answer to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// The questions of one `ask_user` call, checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Form {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub questions: Vec<FormQuestion>,
}

impl Form {
    /// Check the tool's arguments: the form, or every problem, each naming the
    /// question by index and id so a model can fix them all in one more call.
    /// `null` and a missing field are the same, as strict tool modes send
    /// every field.
    pub fn parse(args: &Map<String, Value>) -> Result<Form, Vec<String>> {
        let mut problems = Vec::new();
        for key in args.keys() {
            if key != "title" && key != "questions" {
                problems.push(format!("{key}: not a field of ask_user (title, questions)"));
            }
        }
        let title = match args.get("title") {
            None | Some(Value::Null) => None,
            Some(Value::String(title)) if title.trim().is_empty() => None,
            Some(Value::String(title)) => {
                if title.chars().count() > MAX_TITLE_CHARS {
                    problems.push(format!(
                        "title: longer than {MAX_TITLE_CHARS} characters; shorten it"
                    ));
                }
                Some(title.trim().to_owned())
            }
            Some(_) => {
                problems.push("title: should be a string".into());
                None
            }
        };
        let items = match args.get("questions") {
            Some(Value::Array(items)) => items.as_slice(),
            None | Some(Value::Null) => {
                problems.push("questions: required, a list of 1 to 10 questions".into());
                &[]
            }
            Some(_) => {
                problems.push("questions: should be a list of questions".into());
                &[]
            }
        };
        if items.is_empty() && args.get("questions").is_some_and(Value::is_array) {
            problems.push("questions: empty; ask at least one question".into());
        }
        if items.len() > MAX_QUESTIONS {
            problems.push(format!(
                "questions: {} questions, more than {MAX_QUESTIONS}; ask the {MAX_QUESTIONS} \
                 that matter most now, and the rest in a later call",
                items.len()
            ));
        }
        let mut questions = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if let Some(question) = parse_question(index, item, &mut seen, &mut problems) {
                questions.push(question);
            }
        }
        if problems.is_empty() {
            Ok(Form { title, questions })
        } else {
            Err(problems)
        }
    }

    /// Normalize the answers a surface gave (by question id) to each question's
    /// kind, in the form's order: the answers, which were typed rather than
    /// chosen, and which required questions were left unanswered.
    pub fn accept(&self, values: &Map<String, Value>) -> Accepted {
        let mut accepted = Accepted::default();
        for question in &self.questions {
            let value = values
                .get(&question.id)
                .map(|value| normalize(question, value))
                .unwrap_or(Value::Null);
            if value.is_null() {
                if !question.optional {
                    accepted.unanswered.push(question.id.clone());
                }
            } else if typed(question, &value) {
                accepted.typed.push(question.id.clone());
            }
            accepted.answers.insert(question.id.clone(), value);
        }
        accepted
    }

    /// The kinds asked, in order, for the trace.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.questions.iter().map(|q| q.kind.as_str()).collect()
    }
}

fn parse_question(
    index: usize,
    item: &Value,
    seen: &mut Vec<String>,
    problems: &mut Vec<String>,
) -> Option<FormQuestion> {
    let Some(fields) = item.as_object() else {
        problems.push(format!(
            "questions/{index}: should be an object with id, question, and kind"
        ));
        return None;
    };
    let id = fields.get("id").and_then(Value::as_str).unwrap_or_default();
    let at = if id.is_empty() {
        format!("questions/{index}")
    } else {
        format!("questions/{index} ({id})")
    };
    let before = problems.len();
    for key in fields.keys() {
        if !["id", "question", "kind", "options", "optional"].contains(&key.as_str()) {
            problems.push(format!(
                "{at}: {key} is not a field of a question (id, question, kind, options, optional)"
            ));
        }
    }
    if id.is_empty() {
        problems.push(format!(
            "{at}: id is required, an identifier such as \"scope\""
        ));
    } else if !agent86_ui_identifier(id) {
        problems.push(format!(
            "{at}: id should be an identifier: letters, digits, and _, starting with a letter or _"
        ));
    } else if seen.iter().any(|other| other == id) {
        problems.push(format!(
            "{at}: id {id:?} is used by an earlier question; ids must differ"
        ));
    } else {
        seen.push(id.to_owned());
    }
    let question = match fields.get("question") {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            if text.chars().count() > MAX_QUESTION_CHARS {
                problems.push(format!(
                    "{at}: question is longer than {MAX_QUESTION_CHARS} characters; shorten it"
                ));
            }
            text.trim().to_owned()
        }
        _ => {
            problems.push(format!("{at}: question is required, the text to ask"));
            String::new()
        }
    };
    let kind = match fields.get("kind").and_then(Value::as_str) {
        Some(name) => AnswerKind::parse(name).or_else(|| {
            problems.push(format!("{at}: kind {name:?} is not one of {}", kind_list()));
            None
        }),
        None => {
            problems.push(format!("{at}: kind is required, one of {}", kind_list()));
            None
        }
    };
    let options: Vec<String> = match fields.get("options") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(options)) => options
            .iter()
            .filter_map(|option| match option {
                Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
                _ => {
                    problems.push(format!("{at}: every option should be a non-empty string"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            problems.push(format!("{at}: options should be a list of strings"));
            Vec::new()
        }
    };
    if let Some(kind) = kind {
        if kind.has_options() {
            if options.len() < MIN_OPTIONS || options.len() > MAX_OPTIONS {
                problems.push(format!(
                    "{at}: a {} question needs {MIN_OPTIONS} to {MAX_OPTIONS} options, not {} \
                     (\"Other\" is added for you)",
                    kind.as_str(),
                    options.len()
                ));
            }
            let mut unique: Vec<&String> = Vec::new();
            for option in &options {
                if option.chars().count() > MAX_OPTION_CHARS {
                    problems.push(format!(
                        "{at}: an option is longer than {MAX_OPTION_CHARS} characters; shorten it"
                    ));
                }
                if unique.contains(&option) {
                    problems.push(format!("{at}: the option {option:?} is listed twice"));
                }
                unique.push(option);
            }
        } else if !options.is_empty() {
            problems.push(format!(
                "{at}: options are only for choice and multi questions, not {}",
                kind.as_str()
            ));
        }
    }
    let optional = match fields.get("optional") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(optional)) => *optional,
        Some(_) => {
            problems.push(format!("{at}: optional should be true or false"));
            false
        }
    };
    if problems.len() > before {
        return None;
    }
    Some(FormQuestion {
        id: id.to_owned(),
        question,
        kind: kind?,
        options,
        optional,
    })
}

fn kind_list() -> String {
    AnswerKind::ALL
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// An identifier as A2UI's ids are: letters, digits, and `_`, not starting with
/// a digit. Question ids become component ids and data model keys.
fn agent86_ui_identifier(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// A surface's answer to `question` as its kind wants it, or `null` when it
/// gave none.
fn normalize(question: &FormQuestion, value: &Value) -> Value {
    let text = |value: &Value| match value {
        Value::String(text) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    };
    // A ChoicePicker binds a list even when one may be chosen.
    let single = |value: &Value| match value {
        Value::Array(items) => items.iter().find_map(text),
        other => text(other),
    };
    match question.kind {
        AnswerKind::Choice => single(value).map_or(Value::Null, Value::String),
        AnswerKind::Multi => {
            let items: Vec<Value> = match value {
                Value::Array(items) => items.iter().filter_map(text).map(Value::String).collect(),
                other => text(other).map(Value::String).into_iter().collect(),
            };
            if items.is_empty() {
                Value::Null
            } else {
                Value::Array(items)
            }
        }
        AnswerKind::Text | AnswerKind::LongText => single(value).map_or(Value::Null, Value::String),
        AnswerKind::Number => match value {
            Value::Number(_) => value.clone(),
            other => match single(other) {
                None => Value::Null,
                Some(text) => number(&text).unwrap_or(Value::String(text)),
            },
        },
        AnswerKind::YesNo => match value {
            Value::Bool(_) => value.clone(),
            other => match single(other).map(|text| text.to_ascii_lowercase()) {
                None => Value::Null,
                Some(text) if text == "yes" || text == "true" => Value::Bool(true),
                Some(text) if text == "no" || text == "false" => Value::Bool(false),
                Some(text) => Value::String(text),
            },
        },
        AnswerKind::Date => match single(value) {
            None => Value::Null,
            Some(text) => {
                let date = text.get(..10).filter(|date| is_date(date));
                Value::String(date.map_or(text.clone(), str::to_owned))
            }
        },
    }
}

/// `text` as a JSON number: an integer when it has no fraction.
fn number(text: &str) -> Option<Value> {
    if let Ok(integer) = text.parse::<i64>() {
        return Some(Value::from(integer));
    }
    let float = text.parse::<f64>().ok().filter(|f| f.is_finite())?;
    serde_json::Number::from_f64(float).map(Value::Number)
}

/// `YYYY-MM-DD`, by shape.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// Whether an answer to a question with options is (or holds) something not
/// among them: the user's own words.
fn typed(question: &FormQuestion, value: &Value) -> bool {
    if !question.kind.has_options() {
        return false;
    }
    let listed = |text: &str| question.options.iter().any(|option| option == text);
    match value {
        Value::String(text) => !listed(text),
        Value::Array(items) => items
            .iter()
            .any(|item| item.as_str().is_some_and(|text| !listed(text))),
        _ => false,
    }
}

/// The answers to a [`Form`], normalized ([`Form::accept`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Accepted {
    /// By question id, in the form's order; `null` where none was given.
    pub answers: Map<String, Value>,
    /// The choice questions answered in the user's own words.
    pub typed: Vec<String>,
    /// The required questions left unanswered.
    pub unanswered: Vec<String>,
}

impl Accepted {
    /// The call's result, as the model reads it.
    pub fn text(&self) -> String {
        let mut text = format!(
            "The user answered:\n{}",
            crate::pyjson::python_json_dumps(&Value::Object(self.answers.clone()), false)
        );
        if !self.typed.is_empty() {
            text.push_str(&format!(
                "\nIn their own words, not one of the options: {}.",
                self.typed.join(", ")
            ));
        }
        if !self.unanswered.is_empty() {
            text.push_str(&format!(
                "\nLeft unanswered: {}.",
                self.unanswered.join(", ")
            ));
        }
        text
    }
}

/// The result when the user closed the form without sending it. Not a failure:
/// nothing went wrong, and the circuit breaker must not count it.
pub const DISMISSED: &str = "The user closed the form without answering. Do not ask these \
     questions again in this turn: go on with what you know, or end your turn saying what \
     you still need.";

/// How the questions reached the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Via {
    /// A form agent86 built (the terminal interface, an embedder's renderer, an
    /// ACP client with agent86UI).
    Form,
    /// An ACP client's own form, through agent86's questions extension.
    Extension,
    /// Text, one question at a time (the plain loop).
    Text,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Form => "form",
            Via::Extension => "extension",
            Via::Text => "text",
        }
    }
}

/// What the user did with the questions.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Outcome {
    /// Their answers, by question id, as the surface has them; the loop
    /// normalizes them ([`Form::accept`]).
    Answered(Map<String, Value>),
    /// They closed the questions without sending answers.
    Dismissed,
}

/// A hook's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub outcome: Outcome,
    pub via: Via,
}

/// Asks the user a [`Form`]'s questions. Called on the turn's thread, through
/// the prompt queue.
pub type AskPrompt = Box<dyn Fn(&Form) -> Reply + Send + Sync>;

/// The hook that asks the user, once a surface installs one. Clones share it;
/// a sub-agent's context has its own, empty one, since only the main loop asks.
#[derive(Clone, Default)]
pub struct Asker {
    prompt: Arc<Mutex<Option<Arc<AskPrompt>>>>,
}

impl std::fmt::Debug for Asker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Asker")
            .field("can_ask", &self.can_ask())
            .finish()
    }
}

impl Asker {
    /// Install (or, with `None`, remove) the hook.
    pub fn set_prompt(&self, prompt: Option<AskPrompt>) {
        *self.prompt.lock().unwrap_or_else(PoisonError::into_inner) = prompt.map(Arc::new);
    }

    /// Whether anyone can be asked.
    pub fn can_ask(&self) -> bool {
        self.prompt().is_some()
    }

    /// The hook, if one is installed. The lock is not held while the user answers.
    pub fn prompt(&self) -> Option<Arc<AskPrompt>> {
        self.prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The system-prompt section wherever `ask_user` is offered (ADR 0006). With
/// `render_ui` offered too, it says which tool asks questions.
pub fn prompt_section(render_ui_offered: bool) -> String {
    let mut section = String::from(
        "## Asking the user\n\n\
         When you need information, a preference, or a decision from the user before you \
         can go on, call ask_user with all your questions at once. Do not ask them in your \
         reply. Use choice or multi with options whenever the likely answers are known; \
         \"Other\" is always offered, so the user is never trapped by your list. Ask only \
         what you need, then wait for the answers, which come back as the call's result.",
    );
    if render_ui_offered {
        section.push_str(
            " Forms that ask the user questions go through ask_user, never render_ui; \
             render_ui is for layouts, dashboards, and choices with a presentation of their own.",
        );
    }
    section
}

impl crate::agent::Harness {
    /// Offer `ask_user`, answered by `prompt` (ADR 0006). Each surface calls it
    /// with its own hook, where someone can answer; called again, it replaces
    /// the hook. The caller decides whether to (`[ui] ask_user`), since the
    /// harness reads no config.
    pub fn attach_questions(&mut self, prompt: AskPrompt) -> Result<(), String> {
        if self.registry.get(ASK_USER).is_none() {
            self.registry
                .register(Box::new(crate::tools::ask_user::AskUserTool))?;
        }
        self.context.asker.set_prompt(Some(prompt));
        Ok(())
    }

    /// Ask the questions an `ask_user` call checked, when `result` carries them
    /// ([`ASK_KEY`]), and make the answer its result; any other result is
    /// returned as it is. Asked through the prompt queue like a secret (one
    /// question at a time, declined once the turn is cancelled). The answers go
    /// through the vault and the ingress guardrail, as typed input does, and the
    /// call is recorded as `ask_user`, without them.
    pub(crate) fn ask_questions(
        &self,
        call: &crate::types::ToolCall,
        result: crate::types::ToolResult,
        sid: &str,
    ) -> crate::types::ToolResult {
        use crate::approval::{Question, QuestionKind};
        use crate::tools::{failure, success};
        use serde_json::json;
        let mut result = result;
        let Some(value) = result.metadata.remove(ASK_KEY) else {
            return result;
        };
        let answered = |result: crate::types::ToolResult| crate::types::ToolResult {
            call_id: call.id.clone(),
            ..result
        };
        let (Ok(form), Some(prompt)) = (
            serde_json::from_value::<Form>(value),
            self.context.asker.prompt(),
        ) else {
            return answered(failure(
                ASK_USER,
                "Nobody can answer questions here, so none were asked.".into(),
            ));
        };
        let record = |outcome: &str, via: Option<Via>, unanswered: usize| {
            self.recorder.event(
                sid,
                "ask_user",
                vec![
                    ("questions", json!(form.questions.len())),
                    ("kinds", json!(form.kinds())),
                    ("via", json!(via.map(Via::as_str))),
                    ("outcome", json!(outcome)),
                    ("unanswered", json!(unanswered)),
                ],
            );
        };
        let question = Question {
            tool: Some(ASK_USER.into()),
            call: Some(call.id.clone()),
            ..Question::new(QuestionKind::Ask)
        };
        match self.gate.queue.ask(question, || prompt(&form)) {
            None => {
                record("cancelled", None, 0);
                answered(failure(
                    ASK_USER,
                    "The turn was cancelled before the user answered.".into(),
                ))
            }
            Some(Reply {
                outcome: Outcome::Dismissed,
                via,
            }) => {
                record("dismissed", Some(via), 0);
                answered(success(ASK_USER, DISMISSED.into()))
            }
            Some(Reply {
                outcome: Outcome::Answered(values),
                via,
            }) => {
                let accepted = form.accept(&values);
                let text = self.context.secrets.redact(&accepted.text());
                let report = self.guardrails.inspect_ingress(&text);
                if report.flagged() {
                    self.recorder.event(
                        sid,
                        "guardrail",
                        vec![
                            ("stage", json!("ingress")),
                            ("source", json!(ASK_USER)),
                            ("findings", json!(report.summary())),
                        ],
                    );
                }
                if self.guardrails.should_block(&report) {
                    record("refused", Some(via), accepted.unanswered.len());
                    return answered(failure(
                        ASK_USER,
                        format!(
                            "The user's answers were refused by the input guardrail ({}).",
                            report.summary()
                        ),
                    ));
                }
                record("answered", Some(via), accepted.unanswered.len());
                answered(success(ASK_USER, text))
            }
        }
    }
}

/// The option the built form adds to every choice question, and the value it
/// stands for in the data model.
#[cfg(feature = "agent86ui")]
pub const OTHER_LABEL: &str = "Other";
#[cfg(feature = "agent86ui")]
const OTHER_VALUE: &str = "__other__";

/// The A2UI messages of the form for `form` (ADR 0006): one `createSurface` in
/// `catalog_id` (the basic catalog's components), a Column of the title, each
/// question's input bound to `/answers/<id>` (and an "Other" field to
/// `/other/<id>`), and a Send button whose event carries both. agent86 builds
/// it, so it is always valid; a test checks every kind against the validator.
#[cfg(feature = "agent86ui")]
pub fn form_messages(form: &Form, surface_id: &str, catalog_id: &str) -> Vec<Value> {
    use serde_json::json;
    let mut children: Vec<Value> = Vec::new();
    let mut components: Vec<Value> = Vec::new();
    let mut answers = Map::new();
    let mut other = Map::new();
    if let Some(title) = &form.title {
        children.push(json!("title"));
        components
            .push(json!({"id": "title", "component": "Text", "text": format!("**{title}**")}));
    }
    for question in &form.questions {
        let id = &question.id;
        let input = format!("q_{id}");
        let label = if question.optional {
            format!("{} (optional)", question.question)
        } else {
            question.question.clone()
        };
        let bound = json!({"@path": format!("/answers/{id}")});
        children.push(json!(input));
        let component = match question.kind {
            AnswerKind::Choice | AnswerKind::Multi => {
                let mut options: Vec<Value> = question
                    .options
                    .iter()
                    .map(|option| json!({"label": option, "value": option}))
                    .collect();
                options.push(json!({"label": OTHER_LABEL, "value": OTHER_VALUE}));
                answers.insert(id.clone(), json!([]));
                other.insert(id.clone(), json!(""));
                let variant = if question.kind == AnswerKind::Choice {
                    "mutuallyExclusive"
                } else {
                    "multipleSelection"
                };
                json!({"id": input, "component": "ChoicePicker", "label": label,
                       "variant": variant, "options": options, "value": bound})
            }
            AnswerKind::YesNo => {
                answers.insert(id.clone(), json!([]));
                json!({"id": input, "component": "ChoicePicker", "label": label,
                       "variant": "mutuallyExclusive",
                       "options": [{"label": "Yes", "value": "yes"}, {"label": "No", "value": "no"}],
                       "value": bound})
            }
            AnswerKind::Text | AnswerKind::LongText | AnswerKind::Number => {
                answers.insert(id.clone(), json!(""));
                let variant = match question.kind {
                    AnswerKind::LongText => "longText",
                    AnswerKind::Number => "number",
                    _ => "shortText",
                };
                json!({"id": input, "component": "TextField", "label": label,
                       "variant": variant, "value": bound})
            }
            AnswerKind::Date => {
                answers.insert(id.clone(), json!(""));
                json!({"id": input, "component": "DateTimeInput", "label": label,
                       "enableDate": true, "value": bound})
            }
        };
        components.push(component);
        if question.kind.has_options() {
            let field = format!("o_{id}");
            children.push(json!(field));
            components.push(json!({"id": field, "component": "TextField",
                "label": format!("{OTHER_LABEL}, in your own words"), "variant": "shortText",
                "value": {"@path": format!("/other/{id}")}}));
        }
    }
    children.push(json!("send"));
    components.push(
        json!({"id": "send", "component": "Button", "child": "send_label",
        "variant": "primary",
        "action": {"event": {"name": "answers", "context": {
            "answers": {"@path": "/answers"}, "other": {"@path": "/other"}}}}}),
    );
    components.push(json!({"id": "send_label", "component": "Text", "text": "Send answers"}));
    components.insert(
        0,
        json!({"id": "root", "component": "Column", "children": children}),
    );
    vec![json!({"version": "1.0", "createSurface": {
        "surfaceId": surface_id,
        "catalogId": catalog_id,
        "components": components,
        "dataModel": {"answers": answers, "other": other},
    }})]
}

/// The answers, by question id, from the context of the built form's Send
/// action ([`form_messages`]): "Other" is replaced by what the user typed
/// (dropped when they typed nothing), ready for [`Form::accept`].
#[cfg(feature = "agent86ui")]
pub fn values_from_context(form: &Form, context: &Map<String, Value>) -> Map<String, Value> {
    let answers = context.get("answers").and_then(Value::as_object);
    let other = context.get("other").and_then(Value::as_object);
    let mut values = Map::new();
    for question in &form.questions {
        let Some(value) = answers.and_then(|answers| answers.get(&question.id)) else {
            continue;
        };
        let typed = other
            .and_then(|other| other.get(&question.id))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let value = match value {
            Value::Array(items) if question.kind.has_options() => Value::Array(
                items
                    .iter()
                    .filter_map(|item| match item.as_str() {
                        Some(OTHER_VALUE) => typed.map(|text| Value::String(text.to_owned())),
                        _ => Some(item.clone()),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        values.insert(question.id.clone(), value);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn sample() -> Value {
        json!({"title": "Reorganizing your home folder", "questions": [
            {"id": "scope", "question": "Which content should the plan cover?", "kind": "choice",
             "options": ["Whole home folder", "Personal content only"]},
            {"id": "goal", "question": "What is the main goal?", "kind": "multi",
             "options": ["Reduce clutter", "Find files faster", "Remove old backups"]},
            {"id": "keep", "question": "Which folders must not move?", "kind": "text",
             "optional": true},
            {"id": "notes", "question": "Anything else?", "kind": "long_text", "optional": null},
            {"id": "budget", "question": "How many hours?", "kind": "number"},
            {"id": "archive", "question": "Archive old backups?", "kind": "yes_no"},
            {"id": "by", "question": "Done by when?", "kind": "date", "options": null}
        ]})
    }

    #[test]
    fn a_well_formed_call_parses_with_nulls_taken_as_absent() {
        let form = Form::parse(&args(sample())).unwrap();
        assert_eq!(form.title.as_deref(), Some("Reorganizing your home folder"));
        assert_eq!(form.questions.len(), 7);
        assert!(form.questions[2].optional);
        assert!(!form.questions[3].optional);
        assert_eq!(
            form.kinds(),
            [
                "choice",
                "multi",
                "text",
                "long_text",
                "number",
                "yes_no",
                "date"
            ]
        );
    }

    #[test]
    fn every_problem_is_named_by_index_and_id() {
        let problems = Form::parse(&args(json!({"questions": [
            {"id": "scope", "question": "Which?", "kind": "choice", "options": ["Only one"]},
            {"id": "scope", "question": "Again?", "kind": "text"},
            {"id": "2nd", "question": "Bad id?", "kind": "text"},
            {"id": "when", "question": "When?", "kind": "date", "options": ["now", "later"]},
            {"id": "what", "question": "What?", "kind": "dropdown"},
            {"id": "extra", "question": "Extra?", "kind": "text", "label": "x"}
        ], "wait": true})))
        .unwrap_err();
        assert_eq!(
            problems,
            [
                "wait: not a field of ask_user (title, questions)",
                "questions/0 (scope): a choice question needs 2 to 12 options, not 1 (\"Other\" is added for you)",
                "questions/1 (scope): id \"scope\" is used by an earlier question; ids must differ",
                "questions/2 (2nd): id should be an identifier: letters, digits, and _, starting with a letter or _",
                "questions/3 (when): options are only for choice and multi questions, not date",
                "questions/4 (what): kind \"dropdown\" is not one of choice, multi, text, long_text, number, yes_no, date",
                "questions/5 (extra): label is not a field of a question (id, question, kind, options, optional)",
            ]
        );
    }

    #[test]
    fn too_many_or_no_questions_are_refused() {
        let many: Vec<Value> = (0..11)
            .map(|n| json!({"id": format!("q{n}"), "question": "?", "kind": "text"}))
            .collect();
        let problems = Form::parse(&args(json!({ "questions": many }))).unwrap_err();
        assert!(problems[0].starts_with("questions: 11 questions, more than 10"));
        let problems = Form::parse(&args(json!({"questions": []}))).unwrap_err();
        assert_eq!(problems, ["questions: empty; ask at least one question"]);
        let problems = Form::parse(&args(json!({}))).unwrap_err();
        assert_eq!(
            problems,
            ["questions: required, a list of 1 to 10 questions"]
        );
    }

    #[test]
    fn answers_are_normalized_to_each_kind() {
        let form = Form::parse(&args(sample())).unwrap();
        let accepted = form.accept(&args(json!({
            "scope": ["Personal content only"],
            "goal": ["Reduce clutter", "Archive the photos"],
            "keep": "  ",
            "notes": "Leave .ssh alone.",
            "budget": "2.5",
            "archive": ["yes"],
            "by": "2026-12-01T00:00:00Z",
            "unknown": "ignored"
        })));
        assert_eq!(
            Value::Object(accepted.answers.clone()),
            json!({
                "scope": "Personal content only",
                "goal": ["Reduce clutter", "Archive the photos"],
                "keep": null,
                "notes": "Leave .ssh alone.",
                "budget": 2.5,
                "archive": true,
                "by": "2026-12-01"
            })
        );
        assert_eq!(accepted.typed, ["goal"]);
        assert!(accepted.unanswered.is_empty(), "keep is optional");
        assert_eq!(
            accepted.text(),
            "The user answered:\n{\"scope\": \"Personal content only\", \"goal\": [\"Reduce clutter\", \"Archive the photos\"], \"keep\": null, \"notes\": \"Leave .ssh alone.\", \"budget\": 2.5, \"archive\": true, \"by\": \"2026-12-01\"}\nIn their own words, not one of the options: goal."
        );
    }

    #[test]
    fn required_questions_left_empty_are_listed() {
        let form = Form::parse(&args(sample())).unwrap();
        let accepted = form.accept(&args(json!({"scope": "Whole home folder", "budget": 3})));
        assert_eq!(accepted.unanswered, ["goal", "notes", "archive", "by"]);
        assert!(accepted
            .text()
            .ends_with("\nLeft unanswered: goal, notes, archive, by."));
    }

    #[test]
    fn the_asker_holds_one_hook_shared_by_clones() {
        let asker = Asker::default();
        assert!(!asker.can_ask());
        let clone = asker.clone();
        asker.set_prompt(Some(Box::new(|_| Reply {
            outcome: Outcome::Dismissed,
            via: Via::Text,
        })));
        assert!(clone.can_ask());
        let form = Form::parse(&args(sample())).unwrap();
        assert_eq!(clone.prompt().unwrap()(&form).outcome, Outcome::Dismissed);
        asker.set_prompt(None);
        assert!(!clone.can_ask());
    }

    #[test]
    fn the_section_says_which_tool_asks_when_render_ui_is_offered_too() {
        assert!(prompt_section(false).contains("call ask_user with all your questions at once"));
        assert!(!prompt_section(false).contains("render_ui"));
        assert!(prompt_section(true).contains("never render_ui"));
    }

    #[cfg(feature = "agent86ui")]
    #[test]
    fn the_built_form_is_valid_and_its_send_button_carries_the_answers() {
        use agent86_ui::runtime::{Gesture, Runtime};
        let form = Form::parse(&args(sample())).unwrap();
        let messages = form_messages(&form, "ask_1", agent86_ui::BASIC_CATALOG_ID);
        let session =
            crate::agent86ui::UiSession::new(vec![agent86_ui::Catalog::basic().clone()]).unwrap();
        let report = session.apply(&messages);
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(crate::agent86ui::policy(
            &messages,
            &crate::tools::SandboxPolicy::new(&std::env::temp_dir()).unwrap(),
            false
        )
        .is_empty());

        // Fill it in as a user would, and press Send.
        let mut runtime = Runtime::basic();
        runtime.set_rules(crate::agent86ui::RULES);
        assert!(runtime.apply(&messages).is_ok());
        for (path, value) in [
            ("/answers/scope", json!(["Personal content only"])),
            ("/answers/goal", json!(["Reduce clutter", "__other__"])),
            ("/other/goal", json!("Archive the photos")),
            ("/answers/budget", json!("4")),
            ("/answers/archive", json!(["no"])),
            ("/answers/by", json!("2026-12-01")),
        ] {
            runtime.input("ask_1", path, value).unwrap();
        }
        let send = runtime.node("ask_1", "send").unwrap();
        let action = runtime
            .components("ask_1")
            .unwrap()
            .find(|component| component["id"] == "send")
            .unwrap()["action"]
            .clone();
        let dispatch = runtime
            .dispatch(&action, &Gesture::on(&send, "ask_1"))
            .unwrap();
        let action = dispatch.action.unwrap();
        assert_eq!(action.name, "answers");
        let accepted = form.accept(&values_from_context(&form, &action.context));
        assert_eq!(
            Value::Object(accepted.answers),
            json!({
                "scope": "Personal content only",
                "goal": ["Reduce clutter", "Archive the photos"],
                "keep": null,
                "notes": null,
                "budget": 4,
                "archive": false,
                "by": "2026-12-01"
            })
        );
        assert_eq!(accepted.typed, ["goal"]);
        assert_eq!(accepted.unanswered, ["notes"]);
    }

    #[cfg(feature = "agent86ui")]
    #[test]
    fn other_chosen_with_nothing_typed_is_dropped() {
        let form = Form::parse(&args(sample())).unwrap();
        let values = values_from_context(
            &form,
            &args(json!({"answers": {"scope": ["__other__"]}, "other": {"scope": " "}})),
        );
        assert_eq!(values["scope"], json!([]));
        assert_eq!(form.accept(&values).answers["scope"], Value::Null);
    }
}
