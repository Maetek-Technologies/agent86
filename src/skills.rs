//! Skills, ported from Python's `skills/` package: folders holding a `SKILL.md`
//! with YAML frontmatter, discovered from the project and the user's home.
//!
//! Progressive disclosure: only each skill's name and description sit in the
//! system prompt; the full instructions load when the model calls `use_skill`.
//!
//! Search order (the first root wins a name conflict, so a project skill shadows
//! a user skill of the same name):
//!
//! ```text
//! <workspace>/.agent86/skills   project, agent86's own
//! <workspace>/.claude/skills    project, shared with Claude Code
//! ~/.agent86/skills             user
//! ~/.claude/skills              user, shared with Claude Code
//! [skills] paths                whatever the config adds, in order
//! ```
//!
//! The frontmatter is read with Python's built-in parser (`_mini_frontmatter`),
//! which covers what `SKILL.md` files use: `key: value`, quoted strings, inline
//! and block lists, and `>`/`|` block scalars. Python uses PyYAML instead when it
//! happens to be installed; that is a known difference for exotic YAML. Quoted
//! strings are unescaped as PyYAML reads them, because a typical install has it.

use crate::config::Config;
use crate::difflib::splitlines;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A frontmatter value from the built-in parser: a string or a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    List(Vec<Value>),
}

impl Value {
    /// Python's `str(value)` for what the parser can produce.
    fn python_str(&self) -> String {
        match self {
            Self::Str(text) => text.clone(),
            Self::List(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        Self::Str(text) => crate::types::python_repr(text),
                        list => list.python_str(),
                    })
                    .collect();
                format!("[{}]", parts.join(", "))
            }
        }
    }
}

/// A self-contained skill: metadata always loaded, instructions on demand.
///
/// `allowed_tools` is the Agent Skills `allowed-tools`: when non-empty it is a
/// restriction, and while the skill is active every other tool is refused.
#[derive(Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` file.
    pub path: PathBuf,
    pub allowed_tools: Vec<String>,
    instructions: OnceLock<String>,
}

impl Skill {
    pub fn new(name: &str, description: &str, path: PathBuf, allowed_tools: Vec<String>) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            path,
            allowed_tools,
            instructions: OnceLock::new(),
        }
    }

    /// The skill's folder.
    pub fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new(""))
    }

    /// The instruction body, read when the skill is first used and then kept, as
    /// Python's `Skill.instructions`. An unreadable file reads as empty.
    pub fn instructions(&self) -> &str {
        self.instructions.get_or_init(|| {
            read_text(&self.path)
                .map(|text| parse_skill_md(&text).1)
                .unwrap_or_default()
        })
    }

    /// Names of the files and folders bundled beside `SKILL.md`, sorted.
    pub fn resources(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.directory()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.to_lowercase() != "skill.md")
            .collect();
        names.sort();
        names
    }
}

/// Discovered skills, keyed by name in discovery order.
pub type Skills = indexmap::IndexMap<String, std::sync::Arc<Skill>>;

/// Python's `Path.read_text(encoding="utf-8")`: universal newlines.
fn read_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Split `text` into (raw frontmatter, body), as Python's `split_frontmatter`.
/// The closing `---` (or `...`) must be on its own line; an unterminated
/// frontmatter makes the whole file the body.
pub fn split_frontmatter(text: &str) -> (String, String) {
    let stripped = text.trim_start_matches('\u{feff}');
    let lines = splitlines(stripped);
    if lines.first().is_none_or(|line| python_trim(line) != "---") {
        return (String::new(), stripped.to_owned());
    }
    for index in 1..lines.len() {
        if matches!(python_trim(lines[index]), "---" | "...") {
            let raw = lines[1..index].join("\n");
            let body = lines[index + 1..].join("\n");
            return (raw, body.trim_start_matches('\n').to_owned());
        }
    }
    (String::new(), stripped.to_owned())
}

/// Python's `str.strip()`.
fn python_trim(text: &str) -> &str {
    crate::memory::working::python_strip(text)
}

/// A `SKILL.md` as (frontmatter, body), keys lowercased, as `parse_skill_md`.
pub fn parse_skill_md(text: &str) -> (indexmap::IndexMap<String, Value>, String) {
    let (raw, body) = split_frontmatter(text);
    if python_trim(&raw).is_empty() {
        return (indexmap::IndexMap::new(), body);
    }
    let meta = mini_frontmatter(&raw)
        .into_iter()
        .map(|(key, value)| (python_trim(&key).to_lowercase(), value))
        .collect();
    (meta, body)
}

/// Python's `_mini_frontmatter`: key/value, block scalars, block and inline lists.
fn mini_frontmatter(raw: &str) -> indexmap::IndexMap<String, Value> {
    let mut meta = indexmap::IndexMap::new();
    let lines = splitlines(raw);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if python_trim(line).is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) || !line.contains(':') {
            continue; // a continuation line the handlers below already consumed
        }
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        let key = python_trim(key).to_lowercase();
        let value = python_trim(value);
        let head = value.chars().next();
        if matches!(head, Some('|' | '>'))
            && matches!(
                value.trim_end_matches(|c: char| c == '-' || c == '+' || c.is_ascii_digit()),
                "|" | ">"
            )
        {
            let (block, next) = block_scalar(&lines, i, value.starts_with('>'));
            i = next;
            meta.insert(key, Value::Str(block));
        } else if value.is_empty() {
            let (items, next) = block_list(&lines, i);
            i = next;
            meta.insert(
                key,
                if items.is_empty() {
                    Value::Str(String::new())
                } else {
                    Value::List(items)
                },
            );
        } else {
            meta.insert(key, scalar(value));
        }
    }
    meta
}

/// Python's `len(line) - len(line.lstrip())`: leading whitespace in characters.
fn indent_of(line: &str) -> usize {
    line.chars().count() - line.trim_start().chars().count()
}

fn block_scalar(lines: &[&str], start: usize, folded: bool) -> (String, usize) {
    let mut collected: Vec<String> = Vec::new();
    let mut indent: Option<usize> = None;
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if python_trim(line).is_empty() {
            collected.push(String::new());
            i += 1;
            continue;
        }
        let line_indent = indent_of(line);
        if line_indent == 0 || indent.is_some_and(|indent| line_indent < indent) {
            break;
        }
        let indent = *indent.get_or_insert(line_indent);
        collected.push(line.chars().skip(indent).collect());
        i += 1;
    }
    while collected.last().is_some_and(String::is_empty) {
        collected.pop();
    }
    if !folded {
        return (collected.join("\n"), i);
    }
    // Folded: a paragraph's lines join with a space; blank lines separate paragraphs.
    let mut paragraphs: Vec<Vec<String>> = vec![Vec::new()];
    for entry in collected {
        if !entry.is_empty() {
            paragraphs
                .last_mut()
                .expect("never empty")
                .push(python_trim(&entry).to_owned());
        } else if !paragraphs.last().expect("never empty").is_empty() {
            paragraphs.push(Vec::new());
        }
    }
    let text: Vec<String> = paragraphs
        .iter()
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| paragraph.join(" "))
        .collect();
    (text.join("\n"), i)
}

fn block_list(lines: &[&str], start: usize) -> (Vec<Value>, usize) {
    let mut items = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let stripped = python_trim(lines[i]);
        if stripped.is_empty() {
            i += 1;
            continue;
        }
        if !stripped.starts_with("- ") && stripped != "-" {
            break;
        }
        items.push(scalar(&stripped[1..]));
        i += 1;
    }
    (items, i)
}

/// Unquote a scalar; an inline `[a, b]` becomes a list, as Python's `_scalar`.
fn scalar(value: &str) -> Value {
    let value = python_trim(value);
    if value.starts_with('[') && value.ends_with(']') && value.len() >= 2 {
        return Value::List(
            value[1..value.len() - 1]
                .split(',')
                .filter(|part| !python_trim(part).is_empty())
                .map(scalar)
                .collect(),
        );
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && matches!(chars[0], '\'' | '"') {
        let inner: String = chars[1..chars.len() - 1].iter().collect();
        // Python's mini parser only strips the quotes, but a typical install has
        // PyYAML (huggingface_hub brings it), which reads YAML's escapes:
        // `\"` in a double-quoted description is a quote, `''` in a single-quoted
        // one is an apostrophe. A scalar PyYAML would reject stays as the mini
        // parser leaves it, as Python falls back to that parser.
        let unescaped = if chars[0] == '"' {
            yaml_double_quoted(&inner)
        } else {
            yaml_single_quoted(&inner)
        };
        return Value::Str(unescaped.unwrap_or(inner));
    }
    Value::Str(value.to_owned())
}

/// The inside of a single-quoted YAML scalar: `''` is a quote, and a lone `'`
/// is an error.
fn yaml_single_quoted(inner: &str) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\'' && chars.next() != Some('\'') {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

/// The inside of a double-quoted YAML scalar with YAML 1.1's escapes, as PyYAML
/// reads them; an unknown escape or a bare `"` is an error.
fn yaml_double_quoted(inner: &str) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return None,
            '\\' => {}
            _ => {
                out.push(c);
                continue;
            }
        }
        let escaped = match chars.next()? {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            ' ' => ' ',
            '"' => '"',
            '/' => '/',
            '\\' => '\\',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            kind @ ('x' | 'u' | 'U') => {
                let digits = match kind {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let hex: String = chars.by_ref().take(digits).collect();
                if hex.len() != digits || !hex.chars().all(|d| d.is_ascii_hexdigit()) {
                    return None;
                }
                char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
            }
            _ => return None,
        };
        out.push(escaped);
    }
    Some(out)
}

/// `allowed-tools` as tool names, as Python's `parse_allowed_tools`: space
/// separated by convention, but commas, an inline list, and a block list all work.
pub fn parse_allowed_tools(value: &Value) -> Vec<String> {
    match value {
        Value::List(items) => items.iter().flat_map(parse_allowed_tools).collect(),
        Value::Str(text) => {
            let mut text = python_trim(text);
            if text.starts_with('[') && text.ends_with(']') && text.len() >= 2 {
                text = &text[1..text.len() - 1];
            }
            text.replace(',', " ")
                .split_whitespace()
                .map(|token| python_trim(token).trim_matches(['\'', '"']).to_owned())
                .collect()
        }
    }
}

/// Python's `_as_text`.
fn as_text(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::List(items)) => items
            .iter()
            .map(Value::python_str)
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::Str(text)) => python_trim(text).to_owned(),
    }
}

/// A [`Skill`] from one `SKILL.md`, or `None` when it cannot be read.
pub fn load_skill(skill_md: &Path) -> Option<Skill> {
    let (meta, _) = parse_skill_md(&read_text(skill_md)?);
    let mut name = as_text(meta.get("name"));
    if name.is_empty() {
        name = skill_md
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    let allowed = ["allowed-tools", "allowed_tools"]
        .iter()
        .find_map(|key| meta.get(*key))
        .map(parse_allowed_tools)
        .unwrap_or_default();
    Some(Skill::new(
        &name,
        &as_text(meta.get("description")),
        skill_md.to_path_buf(),
        allowed,
    ))
}

/// Every directory searched for skills, highest precedence first. `home` is the
/// user's home directory (explicit, so tests never read the real one).
pub fn default_skill_paths(config: &Config, workspace: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec![
        workspace.join(".agent86").join("skills"),
        workspace.join(".claude").join("skills"),
    ];
    if let Some(home) = home {
        paths.push(home.join(".agent86").join("skills"));
        paths.push(home.join(".claude").join("skills"));
    }
    paths.extend(
        config
            .skills
            .paths
            .iter()
            .map(|path| crate::session::expand_home(path)),
    );
    paths
}

/// The skill directories that exist, resolved: what the sandbox lets tools read,
/// because a skill's instructions routinely point at files bundled beside it.
pub fn skill_roots(config: &Config, workspace: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    if !config.skills.enabled {
        return Vec::new();
    }
    default_skill_paths(config, workspace, home)
        .into_iter()
        .filter(|path| path.is_dir())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect()
}

/// Python's ordering of `Path`s: case-insensitive on Windows.
fn path_order(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// Every skill across the search paths, keyed by name, the first root winning a
/// conflict, as Python's `discover_skills`.
pub fn discover_skills(config: &Config, workspace: &Path, home: Option<&Path>) -> Skills {
    let mut found = Skills::new();
    if !config.skills.enabled {
        return found;
    }
    for root in default_skill_paths(config, workspace, home) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort_by_key(|path| {
            path_order(&path.file_name().unwrap_or_default().to_string_lossy())
        });
        for entry in entries {
            let skill_md = entry.join("SKILL.md");
            if entry.is_dir() && skill_md.is_file() {
                if let Some(skill) = load_skill(&skill_md) {
                    if !found.contains_key(&skill.name) {
                        found.insert(skill.name.clone(), std::sync::Arc::new(skill));
                    }
                }
            }
        }
    }
    found
}

/// Python's `" ".join(text.split())`: a description flattened onto one line.
pub fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Value {
        Value::Str(text.to_owned())
    }

    #[test]
    fn frontmatter_needs_its_own_closing_line() {
        let (raw, body) = split_frontmatter("\u{feff}---\nname: a\n---\n\n\nBody\n---\nmore");
        assert_eq!(raw, "name: a");
        assert_eq!(body, "Body\n---\nmore");
        // A rule inside the body does not end the frontmatter early.
        let (raw, _) = split_frontmatter("---\ndescription: a---b\n...\nx");
        assert_eq!(raw, "description: a---b");
        // Unterminated: everything is the body.
        assert_eq!(
            split_frontmatter("---\nname: a\n"),
            (String::new(), "---\nname: a\n".to_owned())
        );
        assert_eq!(
            split_frontmatter("no frontmatter"),
            (String::new(), "no frontmatter".to_owned())
        );
    }

    #[test]
    fn the_mini_parser_reads_what_skill_files_use() {
        let raw = "Name: pirate-speak\n\
                   description: >\n  Rewrite text in pirate dialect.\n  Use when asked.\n\n  Second paragraph.\n\
                   literal: |\n  line one\n    indented\n\
                   allowed-tools: read_file write_file\n\
                   inline: [a, 'b', \"c\"]\n\
                   block:\n  - x\n  - 'y'\n\
                   # a comment\n\
                   quoted: \"hello: world\"\n\
                   empty:\n\
                   metadata:\n  version: 1.2.0\n";
        let meta = mini_frontmatter(raw);
        assert_eq!(meta["name"], s("pirate-speak"));
        assert_eq!(
            meta["description"],
            s("Rewrite text in pirate dialect. Use when asked.\nSecond paragraph.")
        );
        assert_eq!(meta["literal"], s("line one\n  indented"));
        assert_eq!(meta["allowed-tools"], s("read_file write_file"));
        assert_eq!(meta["inline"], Value::List(vec![s("a"), s("b"), s("c")]));
        assert_eq!(meta["block"], Value::List(vec![s("x"), s("y")]));
        assert_eq!(meta["quoted"], s("hello: world"));
        assert_eq!(meta["empty"], s(""));
        // Nested mappings are beyond the mini parser: an empty value, its lines skipped.
        assert_eq!(meta["metadata"], s(""));
        assert!(!meta.contains_key("version"));
        // Block-scalar indicators with chomping and indentation markers.
        assert_eq!(mini_frontmatter("d: >-\n  a\n  b\n")["d"], s("a b"));
        assert_eq!(mini_frontmatter("d: |2\n  a\n")["d"], s("a"));
        assert_eq!(mini_frontmatter("d: >x\n")["d"], s(">x"));
    }

    #[test]
    fn quoted_scalars_read_escapes_as_pyyaml_does() {
        // Expected values from PyYAML's `safe_load`.
        assert_eq!(scalar(r#""a \"b\" c""#), s("a \"b\" c"));
        assert_eq!(scalar("'it''s'"), s("it's"));
        assert_eq!(
            scalar(r#""tab\there é \x41 \/ \\ \N\_\e""#),
            s("tab\there \u{e9} A / \\ \u{85}\u{a0}\u{1b}")
        );
        assert_eq!(scalar(r#""\U0001F600""#), s("\u{1f600}"));
        // What PyYAML rejects keeps the mini parser's reading.
        assert_eq!(scalar(r#""bad \q""#), s(r"bad \q"));
        assert_eq!(scalar("'lone ' quote'"), s("lone ' quote"));
        assert_eq!(scalar(r#""a "b" c""#), s(r#"a "b" c"#));
        assert_eq!(scalar(r#""short \x4""#), s(r"short \x4"));
    }

    #[test]
    fn allowed_tools_accept_every_spelling() {
        let expected = vec!["read_file".to_owned(), "write_file".to_owned()];
        assert_eq!(parse_allowed_tools(&s("read_file write_file")), expected);
        assert_eq!(parse_allowed_tools(&s("read_file, write_file")), expected);
        assert_eq!(
            parse_allowed_tools(&s("[read_file, 'write_file']")),
            expected
        );
        assert_eq!(
            parse_allowed_tools(&Value::List(vec![s("read_file"), s("write_file")])),
            expected
        );
    }

    #[test]
    fn descriptions_flatten_onto_one_line() {
        assert_eq!(one_line("  a\n b\t\tc  "), "a b c");
        assert_eq!(as_text(Some(&Value::List(vec![s("a"), s("b")]))), "a b");
        assert_eq!(
            as_text(Some(&Value::List(vec![Value::List(vec![s("a")])]))),
            "['a']"
        );
        assert_eq!(as_text(None), "");
    }
}
