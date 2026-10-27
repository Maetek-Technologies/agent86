//! The configuration side of `/config mcp`, as the pure helpers in Python's
//! `tui/screens/mcp_manager.py`: the server rows (config facts only, so opening
//! the manager never starts anything), the two JSON shapes a README gives, the
//! manual form's fields, and the changes a save writes.
//!
//! Validation is the config model's own ([`McpServerConfig::resolve_transport`]);
//! its message is shown as it is, never reworded.

use crate::config::{Config, McpServerConfig};
use serde_json::{Map, Value};

/// One row of the server list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRow {
    pub name: String,
    pub transport: String,
    /// What `agent86 mcp list` shows: the URL, or the command line.
    pub endpoint: String,
    pub enabled: bool,
}

impl McpRow {
    pub fn label(&self) -> String {
        let status = if self.enabled { "enabled" } else { "disabled" };
        format!(
            "{}  {}  {}  ({status})",
            self.name, self.transport, self.endpoint
        )
    }
}

/// Every configured server, in config order. Never connects to anything.
pub fn mcp_rows(config: &Config) -> Vec<McpRow> {
    config
        .mcp_servers
        .iter()
        .map(|(name, server)| McpRow {
            name: name.clone(),
            transport: server.transport.clone().unwrap_or_else(|| "?".to_owned()),
            endpoint: server.url.clone().unwrap_or_else(|| command_line(server)),
            enabled: server.enabled,
        })
        .collect()
}

/// `command` and `args` joined with spaces, as Python shows and prefills them.
pub fn command_line(server: &McpServerConfig) -> String {
    std::iter::once(server.command.clone().unwrap_or_default())
        .chain(server.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

/// A validated server not yet saved. `original_name` is set when editing.
#[derive(Debug, Clone, PartialEq)]
pub struct McpDraft {
    pub name: String,
    pub config: McpServerConfig,
    pub original_name: Option<String>,
}

/// Python's `repr` of a string, for messages that quote what was typed.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `KEY=value, OTHER=${VAR}` as pairs. Only the first `=` separates, and values are
/// kept as typed, so a `${VAR}` reference reaches the config unresolved.
pub fn parse_kv_list(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let bad = || format!("Expected KEY=value, got: {}", py_repr(part));
        let (key, value) = part.split_once('=').ok_or_else(bad)?;
        let key = key.trim();
        if key.is_empty() {
            return Err(bad());
        }
        // A repeated key keeps its first place and takes the last value, as a dict does.
        match pairs.iter_mut().find(|(known, _)| known == key) {
            Some(pair) => pair.1 = value.trim().to_owned(),
            None => pairs.push((key.to_owned(), value.trim().to_owned())),
        }
    }
    Ok(pairs)
}

/// Build and validate a server from config-shaped fields.
fn validated(fields: Map<String, Value>) -> Result<McpServerConfig, String> {
    let mut config: McpServerConfig =
        serde_json::from_value(Value::Object(fields)).map_err(|error| error.to_string())?;
    config.resolve_transport()?;
    Ok(config)
}

/// A server from a README-shaped object: known fields kept, `type` taken as the
/// transport when it names one, `disabled` as the opposite of `enabled`, and
/// anything else dropped.
fn server_from_object(data: &Map<String, Value>) -> Result<McpServerConfig, String> {
    const KNOWN: &[&str] = &[
        "command",
        "args",
        "env",
        "url",
        "headers",
        "transport",
        "enabled",
    ];
    let mut cleaned = Map::new();
    for (key, value) in data {
        if KNOWN.contains(&key.as_str()) {
            cleaned.insert(key.clone(), value.clone());
        } else if key == "type" {
            if let Some(kind) = value
                .as_str()
                .map(str::to_lowercase)
                .filter(|kind| matches!(kind.as_str(), "stdio" | "sse" | "http"))
            {
                cleaned.insert("transport".to_owned(), Value::String(kind));
            }
        } else if key == "disabled" {
            cleaned.insert("enabled".to_owned(), Value::Bool(!truthy(value)));
        }
    }
    validated(cleaned)
}

/// Python's `bool(value)` for JSON.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Either JSON shape a README gives: a bare server object, named by `name_hint`, or
/// one wrapped as `{"mcpServers": {"name": {...}}}` (or VS Code's `"servers"`), whose
/// key names it.
pub fn parse_server_json(raw: &str, name_hint: &str) -> Result<(String, McpServerConfig), String> {
    let data: Value = serde_json::from_str(raw).map_err(|error| {
        let message = error.to_string();
        let message = message
            .split(" at line ")
            .next()
            .unwrap_or(&message)
            .to_owned();
        format!(
            "Not valid JSON: {message} (line {} col {})",
            error.line(),
            error.column()
        )
    })?;
    let Value::Object(data) = data else {
        return Err("Expected a JSON object.".to_owned());
    };
    if let Some(wrapper) = ["mcpServers", "servers"]
        .into_iter()
        .find(|key| data.contains_key(*key))
    {
        let only = match &data[wrapper] {
            Value::Object(servers) if servers.len() == 1 => servers.iter().next(),
            _ => None,
        };
        let Some((name, server)) = only else {
            return Err(format!("Expected exactly one server under \"{wrapper}\"."));
        };
        let empty = Map::new();
        let server = server.as_object().unwrap_or(&empty);
        return Ok((name.clone(), server_from_object(server)?));
    }
    if name_hint.is_empty() {
        return Err("Enter a name for this server.".to_owned());
    }
    Ok((name_hint.to_owned(), server_from_object(&data)?))
}

/// A server from the manual form. The command line is split as Python's `shlex`
/// does on this platform (POSIX rules, or Windows' quote-keeping ones); its first
/// word is the command. The transport choice counts only with a URL, since http
/// and sse look the same from a URL alone.
pub fn build_manual_config(
    command_line: &str,
    url: &str,
    env: &str,
    headers: &str,
    transport: Option<&str>,
) -> Result<McpServerConfig, String> {
    let (command_line, url) = (command_line.trim(), url.trim());
    let pairs = |pairs: Vec<(String, String)>| -> Value {
        Value::Object(
            pairs
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    };
    let env = pairs(parse_kv_list(env)?);
    let headers = pairs(parse_kv_list(headers)?);
    let mut words = if command_line.is_empty() {
        Vec::new()
    } else {
        shlex_split(command_line, !cfg!(windows))?
    };
    let mut fields = Map::new();
    if !words.is_empty() {
        fields.insert("command".to_owned(), Value::String(words.remove(0)));
    }
    fields.insert(
        "args".to_owned(),
        Value::Array(words.into_iter().map(Value::String).collect()),
    );
    fields.insert("env".to_owned(), env);
    if !url.is_empty() {
        fields.insert("url".to_owned(), Value::String(url.to_owned()));
        if let Some(transport) = transport {
            fields.insert("transport".to_owned(), Value::String(transport.to_owned()));
        }
    }
    fields.insert("headers".to_owned(), headers);
    validated(fields)
}

/// Python's `shlex.split(text, posix=posix)` (no comments).
///
/// POSIX: quotes group and are removed; a backslash escapes the next character
/// outside quotes, and only `"` or `\` inside double quotes. Not POSIX (Windows):
/// quotes group and stay in the word, and backslashes are ordinary.
pub fn shlex_split(text: &str, posix: bool) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\\' if posix => {
                let escaped = chars.next().ok_or("No escaped character")?;
                word.push(escaped);
                in_word = true;
            }
            // Not POSIX: a quote inside a word is an ordinary character.
            '\'' | '"' if !posix && in_word => word.push(ch),
            '\'' | '"' => {
                in_word = true;
                if !posix {
                    word.push(ch);
                }
                loop {
                    let next = chars.next().ok_or("No closing quotation")?;
                    if next == ch {
                        break;
                    }
                    if posix && ch == '"' && next == '\\' {
                        match chars.peek() {
                            Some(&following @ ('"' | '\\')) => {
                                chars.next();
                                word.push(following);
                                continue;
                            }
                            Some(_) => {}
                            None => return Err("No closing quotation".to_owned()),
                        }
                    }
                    word.push(next);
                }
                if !posix {
                    // ...and a quoted word ends at its closing quote.
                    word.push(ch);
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            other => {
                word.push(other);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The TOML a save writes for `draft`. Every `env` and `headers` value is its own
/// key, so the writer's secret guard sees each one; a renamed server's old block
/// is removed in the same edit.
pub fn mcp_changes(draft: &McpDraft) -> Vec<crate::config_writer::Change> {
    use toml_edit::Value as Toml;
    let base = |rest: &[&str]| {
        ["mcp", "servers", draft.name.as_str()]
            .iter()
            .chain(rest)
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>()
    };
    let config = &draft.config;
    let mut changes = Vec::new();
    if let Some(command) = &config.command {
        changes.push((base(&["command"]), Some(Toml::from(command.as_str()))));
        let args: toml_edit::Array = config.args.iter().map(String::as_str).collect();
        changes.push((base(&["args"]), Some(Toml::Array(args))));
    }
    if let Some(url) = &config.url {
        changes.push((base(&["url"]), Some(Toml::from(url.as_str()))));
        changes.push((base(&["transport"]), Some(Toml::from(config.transport()))));
    }
    for (key, value) in &config.env {
        changes.push((base(&["env", key]), Some(Toml::from(value.as_str()))));
    }
    for (key, value) in &config.headers {
        changes.push((base(&["headers", key]), Some(Toml::from(value.as_str()))));
    }
    changes.push((base(&["enabled"]), Some(Toml::from(config.enabled))));
    if let Some(original) = draft
        .original_name
        .as_ref()
        .filter(|original| **original != draft.name)
    {
        let path = ["mcp", "servers", original.as_str()]
            .iter()
            .map(|part| (*part).to_owned())
            .collect();
        changes.push((path, None));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_value_lists_keep_references_and_quote_mistakes() {
        assert_eq!(
            parse_kv_list(" A=1, TOKEN=${GH_TOKEN} ,, URL=a=b ").unwrap(),
            [
                ("A".to_owned(), "1".to_owned()),
                ("TOKEN".to_owned(), "${GH_TOKEN}".to_owned()),
                ("URL".to_owned(), "a=b".to_owned())
            ]
        );
        assert!(parse_kv_list("").unwrap().is_empty());
        assert_eq!(
            parse_kv_list("A=1, oops").unwrap_err(),
            "Expected KEY=value, got: 'oops'"
        );
        assert_eq!(
            parse_kv_list("=1").unwrap_err(),
            "Expected KEY=value, got: '=1'"
        );
        assert_eq!(py_repr("it's"), "\"it's\"");
    }

    #[test]
    fn both_json_shapes_are_accepted() {
        let (name, server) =
            parse_server_json(r#"{"command": "npx", "args": ["-y", "gh"]}"#, "github").unwrap();
        assert_eq!(name, "github");
        assert_eq!(server.transport(), "stdio");
        assert_eq!(server.args, ["-y", "gh"]);

        let wrapped = r#"{"mcpServers": {"docs": {"type": "SSE", "url": "http://x/sse",
            "disabled": true, "unknown": 1}}}"#;
        let (name, server) = parse_server_json(wrapped, "ignored").unwrap();
        assert_eq!((name.as_str(), server.transport()), ("docs", "sse"));
        assert!(!server.enabled);

        assert_eq!(
            parse_server_json(r#"{"servers": {}}"#, "x").unwrap_err(),
            "Expected exactly one server under \"servers\"."
        );
        assert_eq!(
            parse_server_json("[1]", "x").unwrap_err(),
            "Expected a JSON object."
        );
        assert_eq!(
            parse_server_json(r#"{"command": "npx"}"#, "").unwrap_err(),
            "Enter a name for this server."
        );
        assert_eq!(
            parse_server_json(r#"{"command": "a", "url": "http://b"}"#, "x").unwrap_err(),
            "MCP server sets both 'command' and 'url'; use one transport"
        );
        assert!(parse_server_json("{", "x")
            .unwrap_err()
            .starts_with("Not valid JSON: "));
    }

    #[test]
    fn the_manual_form_splits_the_command_line() {
        let server = build_manual_config(
            "npx -y \"@scope/server name\"",
            "",
            "TOKEN=${GH}",
            "",
            Some("sse"),
        )
        .unwrap();
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.transport(), "stdio", "the choice needs a URL");
        assert_eq!(server.env["TOKEN"], "${GH}");
        let words = &server.args;
        if cfg!(windows) {
            assert_eq!(words, &["-y", "\"@scope/server name\""]);
        } else {
            assert_eq!(words, &["-y", "@scope/server name"]);
        }

        let remote = build_manual_config("", "http://h/sse", "", "X=1", Some("sse")).unwrap();
        assert_eq!(
            (remote.transport(), remote.headers["X"].as_str()),
            ("sse", "1")
        );
        assert_eq!(
            build_manual_config("", "", "", "", None).unwrap_err(),
            "MCP server needs 'command' (stdio) or 'url' (sse/http)"
        );
    }

    #[test]
    fn shlex_follows_python_in_both_modes() {
        assert_eq!(
            shlex_split(r#"a 'b c' "d \" e" f\ g"#, true).unwrap(),
            ["a", "b c", "d \" e", "f g"]
        );
        // Each pinned to CPython's output.
        assert_eq!(
            shlex_split(r#"a 'b c' x"y z"w C:\path"#, false).unwrap(),
            ["a", "'b c'", "x\"y", "z\"w", r"C:\path"]
        );
        assert_eq!(shlex_split("a'b'c", true).unwrap(), ["abc"]);
        assert_eq!(shlex_split("a'b'c", false).unwrap(), ["a'b'c"]);
        assert_eq!(shlex_split("\"a\"b c", false).unwrap(), ["\"a\"", "b", "c"]);
        assert_eq!(
            shlex_split(r#""a\\b" 'c\d' e\\f"#, true).unwrap(),
            [r"a\b", r"c\d", r"e\f"]
        );
        assert_eq!(
            shlex_split("a 'b", false).unwrap_err(),
            "No closing quotation"
        );
        assert_eq!(
            shlex_split("a 'b", true).unwrap_err(),
            "No closing quotation"
        );
        assert_eq!(
            shlex_split("a\\", true).unwrap_err(),
            "No escaped character"
        );
    }

    #[test]
    fn a_save_writes_each_secret_as_its_own_key() {
        let (_, config) = parse_server_json(
            r#"{"url": "http://h/mcp", "headers": {"Authorization": "Bearer ${T}"}}"#,
            "remote",
        )
        .unwrap();
        let draft = McpDraft {
            name: "remote".into(),
            config,
            original_name: Some("old".into()),
        };
        let changes: Vec<String> = mcp_changes(&draft)
            .into_iter()
            .map(|(path, value)| {
                format!(
                    "{} = {}",
                    path.join("."),
                    value.map_or("DELETE".to_owned(), |value| value.to_string())
                )
            })
            .collect();
        assert_eq!(
            changes,
            [
                "mcp.servers.remote.url = \"http://h/mcp\"",
                "mcp.servers.remote.transport = \"http\"",
                "mcp.servers.remote.headers.Authorization = \"Bearer ${T}\"",
                "mcp.servers.remote.enabled = true",
                "mcp.servers.old = DELETE",
            ]
        );
    }
}
