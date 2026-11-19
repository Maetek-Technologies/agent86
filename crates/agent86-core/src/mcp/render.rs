//! Model-facing rendering of MCP results, ported from Python's
//! `tools/mcp_content.py` and the `_*_to_text` helpers in `tools/mcp_client.py`.
//! Pure functions over the JSON the server sent; nothing here can fail, and a
//! base64 payload is never inlined.

use serde_json::{json, Map, Value};

fn field(value: &Value, name: &str) -> String {
    match value.get(name) {
        None | Some(Value::Null) => "unknown".to_owned(),
        Some(Value::String(text)) if text.is_empty() => "unknown".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn base64_len(value: &Value, name: &str) -> usize {
    value
        .get(name)
        .and_then(Value::as_str)
        .map_or(0, |data| data.chars().count())
}

fn text_of<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or_default()
}

/// One `tools/call` content block as prompt text. Non-text blocks become named
/// markers; base64 payloads are never inlined.
pub fn render_content_block(block: &Value) -> String {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match kind {
        "image" | "audio" => format!(
            "[{kind} content: mime_type={} base64_len={} — not inlined]",
            field(block, "mimeType"),
            base64_len(block, "data")
        ),
        "resource" => {
            let resource = block.get("resource").cloned().unwrap_or(Value::Null);
            let header = format!(
                "embedded resource uri={} mime_type={}",
                field(&resource, "uri"),
                field(&resource, "mimeType")
            );
            match resource.get("text").and_then(Value::as_str) {
                Some(text) => format!("[{header}]\n{text}"),
                None => format!(
                    "[{header} base64_len={} — not inlined]",
                    base64_len(&resource, "blob")
                ),
            }
        }
        "resource_link" => format!(
            "[resource link uri={} name={} mime_type={} size={}]",
            field(block, "uri"),
            field(block, "name"),
            field(block, "mimeType"),
            field(block, "size")
        ),
        other => format!("[non-text content block: {other} — not inlined]"),
    }
}

/// Cut `text` at `limit` characters with a visible marker naming the setting.
pub fn truncate_result(text: &str, limit: usize, server: &str) -> String {
    let length = text.chars().count();
    if length <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!(
        "{kept}\n…[truncated {} chars: result exceeded max_result_chars={limit} for MCP server \
         '{server}'; raise max_result_chars under [mcp.servers.{server}] to see more]",
        length - limit
    )
}

/// A `tools/call` result as prompt text: every block rendered, joined, capped,
/// and prefixed `ERROR: ` when the server flags it as an error.
pub fn content_to_text(result: &Value, limit: usize, server: &str) -> String {
    let parts: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(render_content_block).collect())
        .unwrap_or_default();
    let body = if parts.is_empty() {
        "(no content)".to_owned()
    } else {
        parts.join("\n")
    };
    let body = truncate_result(&body, limit, server);
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        format!("ERROR: {body}")
    } else {
        body
    }
}

/// One pipe-delimited table row with whitespace-collapsed cells.
fn row(cells: &[&str]) -> String {
    cells
        .iter()
        .map(|cell| cell.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn or_unknown(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}

/// `uri | name | mime_type | description`, one line per resource.
pub fn resources_to_text(resources: &[Value]) -> String {
    if resources.is_empty() {
        return "(no resources)".to_owned();
    }
    let mut lines = vec!["uri | name | mime_type | description".to_owned()];
    for resource in resources {
        lines.push(row(&[
            text_of(resource, "uri"),
            text_of(resource, "name"),
            or_unknown(text_of(resource, "mimeType")),
            text_of(resource, "description"),
        ]));
    }
    lines.join("\n")
}

/// `uri_template | name | mime_type | description`, one line per template.
pub fn resource_templates_to_text(templates: &[Value]) -> String {
    if templates.is_empty() {
        return "(no resource templates)".to_owned();
    }
    let mut lines = vec!["uri_template | name | mime_type | description".to_owned()];
    for template in templates {
        lines.push(row(&[
            text_of(template, "uriTemplate"),
            text_of(template, "name"),
            or_unknown(text_of(template, "mimeType")),
            text_of(template, "description"),
        ]));
    }
    lines.join("\n")
}

/// `resources/read` contents: text verbatim, binary parts as markers (also
/// listed under `binary` in the metadata).
pub fn resource_contents_to_text(contents: &[Value]) -> (String, Map<String, Value>) {
    if contents.is_empty() {
        return ("(no content)".to_owned(), Map::new());
    }
    let mut parts = Vec::new();
    let mut binary = Vec::new();
    for item in contents {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text.to_owned());
            continue;
        }
        let uri = text_of(item, "uri").to_owned();
        let mime_type = or_unknown(text_of(item, "mimeType")).to_owned();
        let length = base64_len(item, "blob");
        parts.push(format!(
            "[binary resource uri={uri} mime_type={mime_type} base64_len={length} — not inlined]"
        ));
        binary.push(json!({"uri": uri, "mime_type": mime_type, "base64_len": length}));
    }
    let mut metadata = Map::new();
    if !binary.is_empty() {
        metadata.insert("binary".to_owned(), Value::Array(binary));
    }
    (parts.join("\n"), metadata)
}

/// `name | arguments | description`, one line per prompt; `*` marks a required argument.
pub fn prompts_to_text(prompts: &[Value]) -> String {
    if prompts.is_empty() {
        return "(no prompts)".to_owned();
    }
    let mut lines = vec!["name | arguments | description".to_owned()];
    for prompt in prompts {
        let arguments: Vec<String> = prompt
            .get("arguments")
            .and_then(Value::as_array)
            .map(|arguments| {
                arguments
                    .iter()
                    .map(|argument| {
                        let required =
                            argument.get("required").and_then(Value::as_bool) == Some(true);
                        format!(
                            "{}{}",
                            text_of(argument, "name"),
                            if required { "*" } else { "" }
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let rendered = if arguments.is_empty() {
            "(none)".to_owned()
        } else {
            arguments.join(", ")
        };
        lines.push(row(&[
            text_of(prompt, "name"),
            &rendered,
            text_of(prompt, "description"),
        ]));
    }
    lines.push(
        "(* = required argument; pass them to get_prompt as the `arguments` object)".to_owned(),
    );
    lines.join("\n")
}

/// The Python SDK's class name for a content block type, as Python's markers show it.
fn content_class(kind: &str) -> &str {
    match kind {
        "text" => "TextContent",
        "image" => "ImageContent",
        "audio" => "AudioContent",
        "resource" => "EmbeddedResource",
        "resource_link" => "ResourceLink",
        other => other,
    }
}

/// `prompts/get` messages as a `[role]: text` transcript; non-text blocks become
/// named markers, listed under `non_text_blocks` in the metadata.
pub fn prompt_messages_to_text(messages: &[Value]) -> (String, Map<String, Value>) {
    if messages.is_empty() {
        return ("(no messages)".to_owned(), Map::new());
    }
    let mut lines = Vec::new();
    let mut non_text = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let role = or_unknown(text_of(message, "role"));
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        if let Some(text) = content.get("text").and_then(Value::as_str) {
            lines.push(format!("[{role}]: {text}"));
            continue;
        }
        let kind = content_class(
            content
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("NoneType"),
        );
        non_text.push(json!({"index": index, "role": role, "type": kind}));
        lines.push(format!(
            "[{role}]: [non-text content block: {kind} — not inlined]"
        ));
    }
    let mut metadata = Map::new();
    if !non_text.is_empty() {
        metadata.insert("non_text_blocks".to_owned(), Value::Array(non_text));
    }
    (lines.join("\n"), metadata)
}

/// One suggestion per line, with the server's `total` and `hasMore` in the metadata.
pub fn completion_to_text(completion: &Value) -> (String, Map<String, Value>) {
    let values: Vec<String> = completion
        .get("values")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut metadata = Map::new();
    metadata.insert("count".to_owned(), json!(values.len()));
    metadata.insert(
        "total".to_owned(),
        completion.get("total").cloned().unwrap_or(Value::Null),
    );
    metadata.insert(
        "has_more".to_owned(),
        completion.get("hasMore").cloned().unwrap_or(Value::Null),
    );
    let text = if values.is_empty() {
        "(no suggestions)".to_owned()
    } else {
        values.join("\n")
    };
    (text, metadata)
}

/// A number as Python's `format(value, "g")`: six significant digits, trailing
/// zeros dropped, scientific notation outside `1e-4 <= |value| < 1e6`.
pub fn format_g(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".to_owned()
        } else if value > 0.0 {
            "inf".to_owned()
        } else {
            "-inf".to_owned()
        };
    }
    // Round to six significant digits first, so the exponent reflects the rounding.
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let strip = |text: String| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text
        }
    };
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        strip(format!("{value:.decimals$}"))
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{}e{sign}{:02}",
            strip(mantissa.to_owned()),
            exponent.unsigned_abs()
        )
    }
}

/// Server-supplied text for a note: one line, at most `limit` characters.
pub fn clip(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let kept: String = flat.chars().take(limit - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_format_like_python_g() {
        for (value, expected) in [
            (120.0, "120"),
            (0.5, "0.5"),
            (2.0, "2"),
            (1e-5, "1e-05"),
            (1234567.0, "1.23457e+06"),
            (999999.5, "1e+06"),
            (0.0001, "0.0001"),
            (123.456789, "123.457"),
            (-3.5, "-3.5"),
        ] {
            assert_eq!(format_g(value), expected, "{value}");
        }
    }

    #[test]
    fn notes_clip_server_text() {
        assert_eq!(clip("a\n  b\tc", 200), "a b c");
        assert_eq!(clip("abcdef", 4), "abc…");
    }
}
