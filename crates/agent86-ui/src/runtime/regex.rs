//! The A2UI regex dialect (the specification's "The `regex` function"): a
//! portable subset of ECMAScript patterns in Unicode mode, a static safety check
//! before anything runs, and search semantics.
//!
//! Patterns come from the agent's model, so they are untrusted. A pattern is
//! parsed here, in full; refused if it uses a construct outside the dialect
//! (backreferences, lookbehind, named groups, `\p{...}`, `\cX`, legacy octal,
//! inline flags) or could backtrack badly (the four safety rules); and only then
//! translated into `fancy-regex` syntax with the dialect's meanings made
//! explicit (`.`, `\d`, `\w`, `\s`, `\b`, `^`, `$`). `fancy-regex` adds a
//! backtracking limit on top, as a last line of defense.

use std::fmt::Write as _;

/// The longest pattern, in code points.
pub const MAX_PATTERN_CODE_POINTS: usize = 256;
/// The longest value matched, in code points; a longer one does not match.
pub const MAX_VALUE_CODE_POINTS: usize = 4_096;

/// Why a pattern was refused, with the specification's reason names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It does not parse in the dialect, or uses a construct the dialect
    /// leaves out.
    UnsupportedSyntax,
    /// Longer than [`MAX_PATTERN_CODE_POINTS`] (safety rule 1).
    PatternTooLong,
    /// A repeated subexpression contains an unbounded quantifier (rule 2).
    NestedQuantifier,
    /// An unbounded repetition of alternatives that can start alike (rule 3).
    OverlappingAlternation,
    /// Adjacent unbounded atoms that can match the same character (rule 4).
    AdjacentOverlap,
}

impl Refusal {
    /// The reason as the specification's suites name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::UnsupportedSyntax => "UNSUPPORTED_SYNTAX",
            Refusal::PatternTooLong => "PATTERN_TOO_LONG",
            Refusal::NestedQuantifier => "NESTED_QUANTIFIER",
            Refusal::OverlappingAlternation => "OVERLAPPING_ALTERNATION",
            Refusal::AdjacentOverlap => "ADJACENT_OVERLAP",
        }
    }
}

/// A set of code points as sorted, disjoint, inclusive ranges.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Set(Vec<(u32, u32)>);

const MAX_CODE_POINT: u32 = 0x10FFFF;

impl Set {
    fn of(ranges: &[(u32, u32)]) -> Self {
        let mut set = Set(ranges.to_vec());
        set.normalize();
        set
    }

    fn char(c: u32) -> Self {
        Set(vec![(c, c)])
    }

    fn normalize(&mut self) {
        self.0.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for &(lo, hi) in &self.0 {
            match merged.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        self.0 = merged;
    }

    fn union(&mut self, other: &Set) {
        self.0.extend_from_slice(&other.0);
        self.normalize();
    }

    fn negate(&self) -> Set {
        let mut out = Vec::new();
        let mut next = 0u32;
        for &(lo, hi) in &self.0 {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi.saturating_add(1);
        }
        if next <= MAX_CODE_POINT {
            out.push((next, MAX_CODE_POINT));
        }
        Set(out)
    }

    fn overlaps(&self, other: &Set) -> bool {
        let (mut i, mut j) = (0, 0);
        while i < self.0.len() && j < other.0.len() {
            let (a, b) = (self.0[i], other.0[j]);
            if a.1 < b.0 {
                i += 1;
            } else if b.1 < a.0 {
                j += 1;
            } else {
                return true;
            }
        }
        false
    }

    fn digit() -> Set {
        Set::of(&[(0x30, 0x39)])
    }

    fn word() -> Set {
        Set::of(&[(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)])
    }

    /// ECMAScript WhiteSpace and LineTerminator.
    fn space() -> Set {
        Set::of(&[
            (0x09, 0x0D),
            (0x20, 0x20),
            (0xA0, 0xA0),
            (0x1680, 0x1680),
            (0x2000, 0x200A),
            (0x2028, 0x2029),
            (0x202F, 0x202F),
            (0x205F, 0x205F),
            (0x3000, 0x3000),
            (0xFEFF, 0xFEFF),
        ])
    }

    /// `.`: anything but the line terminators.
    fn dot() -> Set {
        Set::of(&[(0x0A, 0x0A), (0x0D, 0x0D), (0x2028, 0x2029)]).negate()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Empty,
    Set(Set),
    Start,
    End,
    Boundary {
        negated: bool,
    },
    Look {
        negated: bool,
        body: Box<Node>,
    },
    Group {
        capturing: bool,
        body: Box<Node>,
    },
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        body: Box<Node>,
        min: u32,
        max: Option<u32>,
        lazy: bool,
    },
}

type Parsed<T> = Result<T, Refusal>;

const SYNTAX: &str = "^$\\.*+?()[]{}|/";

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        self.pos += 1;
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn alternation(&mut self) -> Parsed<Node> {
        let mut branches = vec![self.concat()?];
        while self.eat('|') {
            branches.push(self.concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alt(branches)
        })
    }

    fn concat(&mut self) -> Parsed<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.term()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn term(&mut self) -> Parsed<Node> {
        let atom = self.atom()?;
        let Some((min, max)) = self.quantifier()? else {
            return Ok(atom);
        };
        // Unicode mode: assertions and lookaheads cannot be repeated.
        if matches!(
            atom,
            Node::Start | Node::End | Node::Boundary { .. } | Node::Look { .. }
        ) {
            return Err(Refusal::UnsupportedSyntax);
        }
        let lazy = self.eat('?');
        Ok(Node::Repeat {
            body: Box::new(atom),
            min,
            max,
            lazy,
        })
    }

    /// A quantifier, if one follows: `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`.
    fn quantifier(&mut self) -> Parsed<Option<(u32, Option<u32>)>> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(Some((0, None)))
            }
            Some('+') => {
                self.pos += 1;
                Ok(Some((1, None)))
            }
            Some('?') => {
                self.pos += 1;
                Ok(Some((0, Some(1))))
            }
            Some('{') => {
                let start = self.pos;
                self.pos += 1;
                let min = self.digits().ok_or(Refusal::UnsupportedSyntax)?;
                let max = if self.eat(',') {
                    if self.peek() == Some('}') {
                        None
                    } else {
                        Some(self.digits().ok_or(Refusal::UnsupportedSyntax)?)
                    }
                } else {
                    Some(min)
                };
                if !self.eat('}') {
                    self.pos = start;
                    return Err(Refusal::UnsupportedSyntax);
                }
                if max.is_some_and(|max| max < min) {
                    return Err(Refusal::UnsupportedSyntax);
                }
                Ok(Some((min, max)))
            }
            _ => Ok(None),
        }
    }

    fn digits(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        Some(text.parse().unwrap_or(u32::MAX))
    }

    fn atom(&mut self) -> Parsed<Node> {
        let c = self.next().ok_or(Refusal::UnsupportedSyntax)?;
        match c {
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '.' => Ok(Node::Set(Set::dot())),
            '(' => self.group(),
            '[' => self.class().map(Node::Set),
            '\\' => self.escape(),
            ')' | ']' | '{' | '}' | '*' | '+' | '?' => Err(Refusal::UnsupportedSyntax),
            other => Ok(Node::Set(Set::char(other as u32))),
        }
    }

    fn group(&mut self) -> Parsed<Node> {
        let (capturing, look) = if self.eat('?') {
            match self.next() {
                Some(':') => (false, None),
                Some('=') => (false, Some(false)),
                Some('!') => (false, Some(true)),
                // Lookbehind `(?<=`/`(?<!`, named groups `(?<name>`, and inline
                // flags `(?i)` are all outside the dialect.
                _ => return Err(Refusal::UnsupportedSyntax),
            }
        } else {
            (true, None)
        };
        let body = self.alternation()?;
        if !self.eat(')') {
            return Err(Refusal::UnsupportedSyntax);
        }
        Ok(match look {
            Some(negated) => Node::Look {
                negated,
                body: Box::new(body),
            },
            None => Node::Group {
                capturing,
                body: Box::new(body),
            },
        })
    }

    /// An escape outside a class.
    fn escape(&mut self) -> Parsed<Node> {
        let c = self.next().ok_or(Refusal::UnsupportedSyntax)?;
        Ok(match c {
            'b' => Node::Boundary { negated: false },
            'B' => Node::Boundary { negated: true },
            '-' => return Err(Refusal::UnsupportedSyntax),
            _ => Node::Set(self.escape_set(c, false)?),
        })
    }

    /// The set an escape stands for (`c` is the character after `\`).
    fn escape_set(&mut self, c: char, in_class: bool) -> Parsed<Set> {
        Ok(match c {
            'd' => Set::digit(),
            'D' => Set::digit().negate(),
            'w' => Set::word(),
            'W' => Set::word().negate(),
            's' => Set::space(),
            'S' => Set::space().negate(),
            't' => Set::char(0x09),
            'n' => Set::char(0x0A),
            'v' => Set::char(0x0B),
            'f' => Set::char(0x0C),
            'r' => Set::char(0x0D),
            'b' if in_class => Set::char(0x08),
            '-' if in_class => Set::char('-' as u32),
            '0' => {
                if self.peek().is_some_and(|d| d.is_ascii_digit()) {
                    // Legacy octal.
                    return Err(Refusal::UnsupportedSyntax);
                }
                Set::char(0)
            }
            'x' => Set::char(self.hex(2)?),
            'u' => self.unicode_escape()?,
            c if SYNTAX.contains(c) => Set::char(c as u32),
            // Backreferences `\1`-`\9` and `\k<name>`, `\p{...}`, `\cX`, and
            // every other identity escape are outside the dialect.
            _ => return Err(Refusal::UnsupportedSyntax),
        })
    }

    fn hex(&mut self, count: usize) -> Parsed<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let digit = self
                .next()
                .and_then(|c| c.to_digit(16))
                .ok_or(Refusal::UnsupportedSyntax)?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// `\uHHHH` (a surrogate pair written as two escapes is one code point),
    /// or `\u{H...}`.
    fn unicode_escape(&mut self) -> Parsed<Set> {
        let code = if self.eat('{') {
            let start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let text: String = self.chars[start..self.pos].iter().collect();
            if text.is_empty() || !self.eat('}') {
                return Err(Refusal::UnsupportedSyntax);
            }
            let value = u32::from_str_radix(&text, 16).map_err(|_| Refusal::UnsupportedSyntax)?;
            if value > MAX_CODE_POINT {
                return Err(Refusal::UnsupportedSyntax);
            }
            value
        } else {
            let high = self.hex(4)?;
            if (0xD800..=0xDBFF).contains(&high)
                && self.peek() == Some('\\')
                && self.peek_at(1) == Some('u')
            {
                let save = self.pos;
                self.pos += 2;
                match self.hex(4) {
                    Ok(low) if (0xDC00..=0xDFFF).contains(&low) => {
                        0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                    }
                    _ => {
                        self.pos = save;
                        high
                    }
                }
            } else {
                high
            }
        };
        // A lone surrogate: no Rust string holds one, so it matches nothing.
        if (0xD800..=0xDFFF).contains(&code) {
            return Ok(Set::default());
        }
        Ok(Set::char(code))
    }

    /// A character class, after its `[`.
    fn class(&mut self) -> Parsed<Set> {
        let negated = self.eat('^');
        let mut set = Set::default();
        loop {
            let first = match self.next() {
                None => return Err(Refusal::UnsupportedSyntax),
                Some(']') => break,
                Some(c) => self.class_atom(c)?,
            };
            if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                self.pos += 1;
                let c = self.next().ok_or(Refusal::UnsupportedSyntax)?;
                let second = self.class_atom(c)?;
                match (single(&first), single(&second)) {
                    (Some(lo), Some(hi)) if lo <= hi => set.union(&Set::of(&[(lo, hi)])),
                    _ => return Err(Refusal::UnsupportedSyntax),
                }
            } else {
                set.union(&first);
            }
        }
        Ok(if negated { set.negate() } else { set })
    }

    fn class_atom(&mut self, c: char) -> Parsed<Set> {
        if c != '\\' {
            return Ok(Set::char(c as u32));
        }
        let escaped = self.next().ok_or(Refusal::UnsupportedSyntax)?;
        if escaped == 'B' {
            return Err(Refusal::UnsupportedSyntax);
        }
        self.escape_set(escaped, true)
    }
}

/// The one code point a set holds, if it holds exactly one.
fn single(set: &Set) -> Option<u32> {
    match set.0.as_slice() {
        [(lo, hi)] if lo == hi => Some(*lo),
        _ => None,
    }
}

fn unbounded(node: &Node) -> bool {
    match node {
        Node::Repeat { body, max, .. } => max.is_none() || unbounded(body),
        Node::Look { body, .. } | Node::Group { body, .. } => unbounded(body),
        Node::Concat(items) | Node::Alt(items) => items.iter().any(unbounded),
        _ => false,
    }
}

fn nullable(node: &Node) -> bool {
    match node {
        Node::Empty | Node::Start | Node::End | Node::Boundary { .. } | Node::Look { .. } => true,
        Node::Set(_) => false,
        Node::Group { body, .. } => nullable(body),
        Node::Repeat { body, min, .. } => *min == 0 || nullable(body),
        Node::Concat(items) => items.iter().all(nullable),
        Node::Alt(items) => items.iter().any(nullable),
    }
}

/// The characters a node's match can start with.
fn first(node: &Node) -> Set {
    match node {
        Node::Set(set) => set.clone(),
        Node::Group { body, .. } | Node::Repeat { body, .. } => first(body),
        Node::Concat(items) => {
            let mut set = Set::default();
            for item in items {
                set.union(&first(item));
                if !nullable(item) {
                    break;
                }
            }
            set
        }
        Node::Alt(items) => {
            let mut set = Set::default();
            for item in items {
                set.union(&first(item));
            }
            set
        }
        _ => Set::default(),
    }
}

fn unwrap_groups(mut node: &Node) -> &Node {
    while let Node::Group { body, .. } = node {
        node = body;
    }
    node
}

/// The character set of an unbounded-repeated atom, for rule 4: a repeated
/// single-character atom, or a group whose only content is one (a group
/// holding just `\d+` counts as `\d+`).
fn unbounded_atom(node: &Node) -> Option<&Set> {
    let node = unwrap_groups(node);
    match node {
        Node::Repeat {
            body, max: None, ..
        } => match unwrap_groups(body) {
            Node::Set(set) => Some(set),
            _ => None,
        },
        _ => None,
    }
}

fn check(node: &Node) -> Result<(), Refusal> {
    match node {
        Node::Repeat { body, max, .. } => {
            if max.is_none_or(|max| max > 1) && unbounded(body) {
                return Err(Refusal::NestedQuantifier);
            }
            if max.is_none() {
                if let Node::Alt(branches) = unwrap_groups(body) {
                    let firsts: Vec<Set> = branches.iter().map(first).collect();
                    for (i, a) in firsts.iter().enumerate() {
                        if firsts[i + 1..].iter().any(|b| a.overlaps(b)) {
                            return Err(Refusal::OverlappingAlternation);
                        }
                    }
                }
            }
            check(body)
        }
        Node::Concat(items) => {
            for (i, item) in items.iter().enumerate() {
                let Some(set) = unbounded_atom(item) else {
                    continue;
                };
                for later in &items[i + 1..] {
                    if let Some(other) = unbounded_atom(later) {
                        if set.overlaps(other) {
                            return Err(Refusal::AdjacentOverlap);
                        }
                    }
                    if !nullable(later) {
                        break;
                    }
                }
            }
            items.iter().try_for_each(check)
        }
        Node::Alt(items) => items.iter().try_for_each(check),
        Node::Look { body, .. } | Node::Group { body, .. } => check(body),
        _ => Ok(()),
    }
}

fn emit_set(set: &Set, out: &mut String) {
    if set.0.is_empty() {
        // Matches nothing.
        out.push_str("(?!)");
        return;
    }
    out.push('[');
    for &(lo, hi) in &set.0 {
        let _ = write!(out, "\\x{{{lo:X}}}");
        if hi != lo {
            let _ = write!(out, "-\\x{{{hi:X}}}");
        }
    }
    out.push(']');
}

const WORD: &str = "[0-9A-Za-z_]";

fn emit(node: &Node, out: &mut String) {
    match node {
        Node::Empty => {}
        Node::Set(set) => emit_set(set, out),
        Node::Start => out.push_str("\\A"),
        Node::End => out.push_str("\\z"),
        // `\b` and `\B` are ASCII-only in the dialect, so spell them out.
        Node::Boundary { negated: false } => {
            let _ = write!(out, "(?:(?<={WORD})(?!{WORD})|(?<!{WORD})(?={WORD}))");
        }
        Node::Boundary { negated: true } => {
            let _ = write!(out, "(?:(?<={WORD})(?={WORD})|(?<!{WORD})(?!{WORD}))");
        }
        Node::Look { negated, body } => {
            out.push_str(if *negated { "(?!" } else { "(?=" });
            emit(body, out);
            out.push(')');
        }
        Node::Group { capturing, body } => {
            out.push_str(if *capturing { "(" } else { "(?:" });
            emit(body, out);
            out.push(')');
        }
        Node::Concat(items) => items.iter().for_each(|item| emit(item, out)),
        Node::Alt(items) => {
            out.push_str("(?:");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push('|');
                }
                emit(item, out);
            }
            out.push(')');
        }
        Node::Repeat {
            body,
            min,
            max,
            lazy,
        } => {
            out.push_str("(?:");
            emit(body, out);
            out.push(')');
            match max {
                None => {
                    let _ = write!(out, "{{{min},}}");
                }
                Some(max) => {
                    let _ = write!(out, "{{{min},{max}}}");
                }
            }
            if *lazy {
                out.push('?');
            }
        }
    }
}

/// A pattern that passed the dialect's checks.
#[derive(Debug)]
pub struct Pattern {
    regex: fancy_regex::Regex,
}

impl Pattern {
    /// Parse and check `pattern`, refusing it with the specification's reason
    /// when it is outside the dialect or unsafe.
    pub fn compile(pattern: &str) -> Result<Pattern, Refusal> {
        if pattern.chars().count() > MAX_PATTERN_CODE_POINTS {
            return Err(Refusal::PatternTooLong);
        }
        let mut parser = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
        };
        let node = parser.alternation()?;
        if parser.pos < parser.chars.len() {
            // A `)` with no group open.
            return Err(Refusal::UnsupportedSyntax);
        }
        check(&node)?;
        let mut translated = String::new();
        emit(&node, &mut translated);
        let regex = fancy_regex::RegexBuilder::new(&translated)
            .backtrack_limit(100_000)
            .build()
            .map_err(|_| Refusal::UnsupportedSyntax)?;
        Ok(Pattern { regex })
    }

    /// Whether the pattern matches anywhere in `value` (`RegExp.test`). A
    /// value longer than [`MAX_VALUE_CODE_POINTS`] does not match; neither
    /// does one that exhausts the backtracking limit.
    pub fn test(&self, value: &str) -> bool {
        if value.chars().count() > MAX_VALUE_CODE_POINTS {
            return false;
        }
        self.regex.is_match(value).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(pattern: &str) -> Option<Refusal> {
        Pattern::compile(pattern).err()
    }

    #[test]
    fn the_specifications_examples() {
        for accepted in [r"^[a-z]+@[a-z]+\.com$", r"^(?=.*\d).{8,}$", r"\w+\s+\w+"] {
            assert_eq!(refusal(accepted), None, "{accepted}");
        }
        assert_eq!(refusal("(a+)+$"), Some(Refusal::NestedQuantifier));
        assert_eq!(refusal("(a|ab)*c"), Some(Refusal::OverlappingAlternation));
        assert_eq!(refusal(r"\d+\s*\d+"), Some(Refusal::AdjacentOverlap));
        assert_eq!(refusal(r"(\w)\1"), Some(Refusal::UnsupportedSyntax));
    }

    #[test]
    fn unicode_mode_refuses_what_legacy_mode_allows() {
        for refused in [
            r"\a", "a{", "]", "}", r"\-", "[a-\\d]", r"\c1", r"\01", "(?i)a", "(?<n>a)",
        ] {
            assert_eq!(
                refusal(refused),
                Some(Refusal::UnsupportedSyntax),
                "{refused}"
            );
        }
        assert_eq!(refusal(r"[\-a]"), None);
        assert_eq!(refusal(r"[\b]"), None);
        assert!(Pattern::compile("^$").unwrap().test(""));
    }

    #[test]
    fn matching_follows_the_dialect() {
        let test = |pattern: &str, value: &str| Pattern::compile(pattern).unwrap().test(value);
        assert!(test("abc", "xxabcxx"));
        assert!(!test("^abc$", "abc\n"));
        assert!(!test(r"^\d+$", "١٢٣"));
        assert!(!test(r"^\w+$", "café"));
        assert!(!test("^a.b$", "a\nb"));
        assert!(!test(r"\b", "é"));
        assert!(test("^.$", "😀"));
        assert!(test(r"^\u{1F600}+$", "😀😀"));
        assert!(test(r"^😀$", "😀"));
        assert!(test("[^]", "x"));
        assert!(!test("[]", "x"));
        assert!(test("a{2}", "xaa"));
        assert!(!test("^a{2}$", "aaa"));
    }
}
