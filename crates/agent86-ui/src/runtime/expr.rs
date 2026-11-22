//! `formatString`'s template language: literal text with `${...}`
//! interpolations, each a literal, a data binding, or a function call.
//!
//! The grammar is the one every A2UI SDK parses (the specification's "The
//! `formatString` function" and the shared suite `core/expressions.yaml`):
//!
//! - `\${` is a literal `${`; an unclosed `${` is an error.
//! - Inside `${...}`: a nested `${...}`, a quoted string (`'...'` or `"..."`,
//!   with `\n`, `\t`, `\r`, and `\x` for any other `x`), a number
//!   (`[+-]?(digits[.digits]|.digits)([eE][+-]?digits)?`, finite), `true`,
//!   `false`, `null` (which reads as `""`), a path (characters that are
//!   `XID_Continue`, `/`, `.`, or `-`; relative unless it starts with `/`), or a
//!   call: a name then `(name: value, ...)`, the arguments themselves
//!   expressions. `@`-names (`@index`) must be calls.
//! - At most [`MAX_DEPTH`] levels of nesting, [`MAX_TEMPLATE_LENGTH`] UTF-16
//!   units (as JavaScript counts them), and [`MAX_PARTS`] parts, so every SDK
//!   accepts and refuses the same templates.

use serde_json::{Map, Number, Value};
use std::sync::OnceLock;

/// The deepest nesting of interpolations and call arguments.
pub const MAX_DEPTH: usize = 100;
/// The longest template, in UTF-16 code units.
pub const MAX_TEMPLATE_LENGTH: usize = 10_000;
/// The most parts one template may produce.
pub const MAX_PARTS: usize = 1_000;

/// A template that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ExpressionError(pub String);

fn fail<T>(message: impl Into<String>) -> Result<T, ExpressionError> {
    Err(ExpressionError(message.into()))
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// A literal: text, a number, or a boolean (`null` is parsed as `""`).
    Literal(Value),
    /// A data binding, absolute or relative.
    Path(String),
    /// A function call with named arguments.
    Call {
        /// The function's name (`@index` included).
        name: String,
        /// The arguments, in the order written.
        args: Vec<(String, Part)>,
    },
}

impl Part {
    /// The part in the shared suite's JSON form: a literal as itself, a
    /// binding as `{"path": ...}`, a call as `{"call", "args", "returnType"}`.
    pub fn to_json(&self) -> Value {
        match self {
            Part::Literal(value) => value.clone(),
            Part::Path(path) => serde_json::json!({ "path": path }),
            Part::Call { name, args } => {
                let args: Map<String, Value> =
                    args.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
                serde_json::json!({ "call": name, "args": args, "returnType": "any" })
            }
        }
    }
}

/// JavaScript's `\s`: ECMAScript WhiteSpace and LineTerminator.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn is_id_continue(c: char) -> bool {
    static XID: OnceLock<regex::Regex> = OnceLock::new();
    let xid = XID.get_or_init(|| regex::Regex::new(r"^\p{XID_Continue}$").expect("XID pattern"));
    let mut buffer = [0u8; 4];
    xid.is_match(c.encode_utf8(&mut buffer))
}

struct Scanner {
    chars: Vec<char>,
    pos: usize,
}

impl Scanner {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek(0);
        self.pos += 1;
        c
    }

    fn matches(&self, expected: &str) -> bool {
        expected
            .chars()
            .enumerate()
            .all(|(i, c)| self.peek(i) == Some(c))
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek(0) == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        while self.peek(0).is_some_and(is_space) {
            self.pos += 1;
        }
    }

    fn keyword(&mut self, word: &str) -> bool {
        let n = word.chars().count();
        if self.matches(word) && !self.peek(n).is_some_and(is_id_continue) {
            self.pos += n;
            true
        } else {
            false
        }
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }

    fn rest(&self) -> String {
        self.slice(self.pos.min(self.chars.len()), self.chars.len())
    }
}

/// Parse a template into its parts. Adjacent text stays one literal; empty
/// literals are dropped.
pub fn parse(template: &str) -> Result<Vec<Part>, ExpressionError> {
    parse_at(template, 0)
}

fn parse_at(template: &str, depth: usize) -> Result<Vec<Part>, ExpressionError> {
    if depth > MAX_DEPTH {
        return fail("Max recursion depth reached in parse");
    }
    let length = template.encode_utf16().count();
    if length > MAX_TEMPLATE_LENGTH {
        return fail(format!(
            "Expression template length ({length}) exceeds maximum limit ({MAX_TEMPLATE_LENGTH})"
        ));
    }
    if template.is_empty() {
        return Ok(Vec::new());
    }
    if !template.contains("${") {
        return Ok(vec![Part::Literal(Value::String(template.into()))]);
    }
    let mut parts = Vec::new();
    let mut scanner = Scanner::new(template);
    while !scanner.at_end() {
        if parts.len() >= MAX_PARTS {
            return fail(format!(
                "Expression parts count exceeds maximum limit ({MAX_PARTS})"
            ));
        }
        if scanner.matches("${") {
            scanner.pos += 2;
            let content = interpolation(&mut scanner)?;
            parts.push(expression(&content, depth + 1)?);
        } else if scanner.matches("\\${") {
            scanner.pos += 3;
            parts.push(Part::Literal(Value::String("${".into())));
        } else {
            let start = scanner.pos;
            while !scanner.at_end() && !scanner.matches("${") && !scanner.matches("\\${") {
                scanner.pos += 1;
            }
            parts.push(Part::Literal(Value::String(
                scanner.slice(start, scanner.pos),
            )));
        }
    }
    Ok(join(parts))
}

/// Join adjacent text literals and drop empty ones.
fn join(parts: Vec<Part>) -> Vec<Part> {
    let mut out: Vec<Part> = Vec::new();
    for part in parts {
        if let Part::Literal(Value::String(text)) = &part {
            if text.is_empty() {
                continue;
            }
            if let Some(Part::Literal(Value::String(previous))) = out.last_mut() {
                previous.push_str(text);
                continue;
            }
        }
        out.push(part);
    }
    out
}

/// The text of an interpolation, after its `${`, up to its matching `}`
/// (braces inside quoted strings do not count).
fn interpolation(scanner: &mut Scanner) -> Result<String, ExpressionError> {
    let start = scanner.pos;
    let mut balance = 1;
    while let Some(c) = scanner.advance() {
        match c {
            '{' => balance += 1,
            '}' => {
                balance -= 1;
                if balance == 0 {
                    return Ok(scanner.slice(start, scanner.pos - 1));
                }
            }
            '\'' | '"' => {
                while let Some(inner) = scanner.advance() {
                    if inner == '\\' {
                        scanner.advance();
                    } else if inner == c {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    fail("Unclosed interpolation: missing '}'")
}

fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

fn expression(text: &str, depth: usize) -> Result<Part, ExpressionError> {
    let text = trim(text);
    if text.is_empty() {
        return Ok(Part::Literal(Value::String(String::new())));
    }
    let mut scanner = Scanner::new(text);
    let part = value(&mut scanner, depth)?;
    if !scanner.at_end() {
        return fail(format!(
            "Unexpected characters at end of expression: '{}'",
            scanner.rest()
        ));
    }
    Ok(part)
}

fn value(scanner: &mut Scanner, depth: usize) -> Result<Part, ExpressionError> {
    if depth > MAX_DEPTH {
        return fail("Max recursion depth reached in parse");
    }
    scanner.skip_space();
    if scanner.at_end() {
        return Ok(Part::Literal(Value::String(String::new())));
    }
    if scanner.matches("${") {
        scanner.pos += 2;
        let content = interpolation(scanner)?;
        return expression(&content, depth + 1);
    }
    if matches!(scanner.peek(0), Some('\'' | '"')) {
        return string(scanner).map(|s| Part::Literal(Value::String(s)));
    }
    if number_start(scanner) {
        return number(scanner).map(Part::Literal);
    }
    if scanner.keyword("true") {
        return Ok(Part::Literal(Value::Bool(true)));
    }
    if scanner.keyword("false") {
        return Ok(Part::Literal(Value::Bool(false)));
    }
    if scanner.keyword("null") {
        return Ok(Part::Literal(Value::String(String::new())));
    }
    let token = path_or_name(scanner)?;
    scanner.skip_space();
    if scanner.peek(0) == Some('(') {
        return call(token, scanner, depth);
    }
    if token.starts_with('@') {
        return fail(format!("Expected '(' after function name '{token}'"));
    }
    if token.is_empty() {
        return Ok(Part::Literal(Value::String(String::new())));
    }
    Ok(Part::Path(token))
}

fn path_or_name(scanner: &mut Scanner) -> Result<String, ExpressionError> {
    let start = scanner.pos;
    if scanner.peek(0) == Some('@') {
        if !scanner
            .peek(1)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return fail("Invalid identifier starting with '@' in expression");
        }
        scanner.pos += 1;
        while scanner
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            scanner.pos += 1;
        }
        return Ok(scanner.slice(start, scanner.pos));
    }
    while scanner
        .peek(0)
        .is_some_and(|c| is_id_continue(c) || matches!(c, '/' | '.' | '-'))
    {
        scanner.pos += 1;
    }
    Ok(scanner.slice(start, scanner.pos))
}

fn call(name: String, scanner: &mut Scanner, depth: usize) -> Result<Part, ExpressionError> {
    scanner.eat('(');
    scanner.skip_space();
    let mut args: Vec<(String, Part)> = Vec::new();
    while !scanner.at_end() && scanner.peek(0) != Some(')') {
        let start = scanner.pos;
        while scanner.peek(0).is_some_and(is_id_continue) {
            scanner.pos += 1;
        }
        let arg = scanner.slice(start, scanner.pos);
        scanner.skip_space();
        if !scanner.eat(':') {
            return fail(format!(
                "Expected ':' after argument name '{arg}' in function '{name}'"
            ));
        }
        scanner.skip_space();
        let parsed = value(scanner, depth + 1)?;
        match args.iter_mut().find(|(existing, _)| *existing == arg) {
            Some(slot) => slot.1 = parsed,
            None => args.push((arg, parsed)),
        }
        scanner.skip_space();
        if scanner.eat(',') {
            scanner.skip_space();
        }
    }
    if !scanner.eat(')') {
        return fail(format!(
            "Expected ')' after function arguments for '{name}'"
        ));
    }
    Ok(Part::Call { name, args })
}

fn string(scanner: &mut Scanner) -> Result<String, ExpressionError> {
    let quote = scanner.advance().unwrap_or('\'');
    let mut out = String::new();
    while let Some(c) = scanner.advance() {
        if c == '\\' {
            match scanner.advance() {
                None => break,
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
            }
        } else if c == quote {
            return Ok(out);
        } else {
            out.push(c);
        }
    }
    fail(format!("Unclosed string literal: missing closing {quote}"))
}

fn number_start(scanner: &Scanner) -> bool {
    let offset = usize::from(matches!(scanner.peek(0), Some('+' | '-')));
    match scanner.peek(offset) {
        Some(c) if c.is_ascii_digit() => true,
        Some('.') => scanner.peek(offset + 1).is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    }
}

fn number(scanner: &mut Scanner) -> Result<Value, ExpressionError> {
    static LITERAL: OnceLock<regex::Regex> = OnceLock::new();
    let literal = LITERAL.get_or_init(|| {
        regex::Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$")
            .expect("number pattern")
    });
    let start = scanner.pos;
    if matches!(scanner.peek(0), Some('+' | '-')) {
        scanner.pos += 1;
    }
    while scanner
        .peek(0)
        .is_some_and(|c| c.is_ascii_digit() || c == '.')
    {
        scanner.pos += 1;
    }
    if matches!(scanner.peek(0), Some('e' | 'E')) {
        scanner.pos += 1;
        if matches!(scanner.peek(0), Some('+' | '-')) {
            scanner.pos += 1;
        }
        while scanner.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            scanner.pos += 1;
        }
    }
    let text = scanner.slice(start, scanner.pos);
    if !literal.is_match(&text) {
        return fail(format!("Invalid number literal: '{text}'"));
    }
    let parsed: f64 = text
        .parse()
        .map_err(|_| ExpressionError(format!("Invalid number literal: '{text}'")))?;
    if !parsed.is_finite() {
        return fail(format!("Number literal is out of range: '{text}'"));
    }
    Ok(number_value(parsed))
}

/// A float as JSON, the way JavaScript would print it: a whole number within
/// `i64` is an integer (and `-0` is `0`), anything else a float.
pub fn number_value(value: f64) -> Value {
    if value == 0.0 {
        return Value::from(0);
    }
    if value.fract() == 0.0 && value.abs() < 9.2e18 {
        // A whole number that fits: cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        return Value::from(value as i64);
    }
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}
