//! The Agent Client Protocol server, beyond Python (`agent86 acp`).
//!
//! ACP lets an editor launch an agent as a subprocess and talk JSON-RPC 2.0 to it
//! over stdio: one message per line, UTF-8, no embedded newlines. The agent must
//! write nothing to stdout that is not a protocol message, so logging goes to
//! stderr and nothing else in this process may print to stdout in this mode.
//!
//! This is slice 1 of the plan: the transport and the `initialize` handshake.
//! Sessions, prompts, permissions, and cancellation come in later slices, so for
//! now every other method answers "method not found" rather than pretending, and
//! the capabilities advertised are only what is built.
//!
//! The pure core is [`Server::handle_line`], which turns one inbound line into the
//! messages to send back, so the protocol is testable without a process. [`serve`]
//! wraps it in the read and write loop.

use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};

/// The ACP major version this server speaks. Version negotiation: an agent that
/// supports the requested version answers with it, and otherwise with the latest
/// it supports, which is this one.
pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC's reserved error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// The protocol state of one connection.
#[derive(Debug, Default)]
pub struct Server {
    initialized: bool,
}

impl Server {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the client has completed `initialize`.
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    /// Handle one inbound line and return the messages to write back, in order.
    /// A blank line, a notification, and a response from the client produce none.
    pub fn handle_line(&mut self, line: &str) -> Vec<Value> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return Vec::new();
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => return vec![error_response(Value::Null, PARSE_ERROR, "Parse error")],
        };
        // ACP does not use JSON-RPC batches, so an array is not a request.
        let Value::Object(object) = message else {
            return vec![error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid request",
            )];
        };
        self.handle_object(object)
    }

    fn handle_object(&mut self, object: Map<String, Value>) -> Vec<Value> {
        let id = object.get("id").cloned();
        let valid_id = matches!(
            id,
            None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Number(_))
        );
        let method = object.get("method").and_then(Value::as_str);
        match (method, id) {
            (Some(method), Some(id)) if valid_id => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                vec![self.request(method, params, id)]
            }
            // A notification has no id and gets no reply, even when unknown.
            (Some(_), None) => Vec::new(),
            // A response to a request this server sent. Slice 1 sends none.
            (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => {
                Vec::new()
            }
            _ => vec![error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid request",
            )],
        }
    }

    fn request(&mut self, method: &str, params: Value, id: Value) -> Value {
        match method {
            "initialize" => match initialize(&params) {
                Ok(result) => {
                    self.initialized = true;
                    success_response(id, result)
                }
                Err(message) => error_response(id, INVALID_PARAMS, &message),
            },
            other => error_response(id, METHOD_NOT_FOUND, &format!("Method not found: {other}")),
        }
    }
}

/// The `initialize` result. The capabilities are exactly what is implemented, and
/// a client treats an omitted capability as unsupported.
fn initialize(params: &Value) -> Result<Value, String> {
    let Some(params) = params.as_object() else {
        return Err("Invalid params: expected an object".into());
    };
    // The requested version is "the latest the client supports". Whatever it is,
    // the answer is the version this server speaks: the same one if it matches,
    // else the latest supported. It still has to be a whole number.
    if params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err("Invalid params: protocolVersion must be an integer".into());
    }
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false
            }
        },
        "agentInfo": {
            "name": "agent86",
            "title": "agent86",
            "version": env!("CARGO_PKG_VERSION")
        },
        "authMethods": []
    }))
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Read newline-delimited messages from `input` until it ends, writing each reply
/// to `output` and flushing it, since the client is waiting on it. End of input
/// is the client closing the connection, so it ends cleanly.
pub fn serve<R: BufRead, W: Write>(mut input: R, mut output: W) -> io::Result<()> {
    let mut server = Server::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if input.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(());
        }
        let replies = match std::str::from_utf8(&buffer) {
            Ok(line) => server.handle_line(line),
            // Messages must be UTF-8; anything else is not valid JSON.
            Err(_) => vec![error_response(Value::Null, PARSE_ERROR, "Parse error")],
        };
        for reply in replies {
            // `to_string` is compact and escapes newlines inside strings, so a
            // message is always exactly one line.
            writeln!(output, "{}", serde_json::to_string(&reply)?)?;
        }
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(server: &mut Server, line: &str) -> Value {
        let mut replies = server.handle_line(line);
        assert_eq!(
            replies.len(),
            1,
            "expected one reply to {line}: {replies:?}"
        );
        replies.remove(0)
    }

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true}},"clientInfo":{"name":"zed","version":"1"}}}"#;

    #[test]
    fn initialize_answers_with_the_version_and_only_what_is_built() {
        let mut server = Server::new();
        let reply = one(&mut server, INITIALIZE);
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 0);
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["authMethods"], json!([]));
        assert_eq!(result["agentInfo"]["name"], "agent86");
        assert_eq!(result["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
        let capabilities = &result["agentCapabilities"];
        assert_eq!(capabilities["loadSession"], false);
        assert_eq!(capabilities["promptCapabilities"]["image"], false);
        assert_eq!(capabilities["promptCapabilities"]["audio"], false);
        assert_eq!(capabilities["promptCapabilities"]["embeddedContext"], false);
        assert_eq!(capabilities["mcpCapabilities"]["http"], false);
        assert_eq!(capabilities["mcpCapabilities"]["sse"], false);
        assert!(server.initialized());
    }

    #[test]
    fn a_newer_client_version_gets_the_latest_this_server_speaks() {
        let mut server = Server::new();
        let reply = one(
            &mut server,
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":7}}"#,
        );
        assert_eq!(reply["id"], "a");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_needs_an_integer_version() {
        for params in [
            r#""params":{}"#,
            r#""params":{"protocolVersion":"1"}"#,
            r#""params":{"protocolVersion":1.5}"#,
            r#""params":[1]"#,
            r#""other":1"#,
        ] {
            let mut server = Server::new();
            let line = format!(r#"{{"jsonrpc":"2.0","id":3,"method":"initialize",{params}}}"#);
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{params}");
            assert_eq!(reply["id"], 3);
            assert!(!server.initialized(), "{params}");
        }
    }

    #[test]
    fn methods_not_built_yet_say_so() {
        let mut server = Server::new();
        for method in ["session/new", "session/prompt", "nonsense"] {
            let line = format!(r#"{{"jsonrpc":"2.0","id":9,"method":"{method}","params":{{}}}}"#);
            let reply = one(&mut server, &line);
            assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
            assert_eq!(
                reply["error"]["message"],
                format!("Method not found: {method}")
            );
        }
    }

    #[test]
    fn notifications_and_client_responses_get_no_reply() {
        let mut server = Server::new();
        for line in [
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#,
            r#"{"jsonrpc":"2.0","method":"unknown"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#,
            "",
            "   \r\n",
        ] {
            assert!(server.handle_line(line).is_empty(), "{line:?}");
        }
    }

    #[test]
    fn bad_input_gets_the_json_rpc_errors_with_a_null_id() {
        let mut server = Server::new();
        let reply = one(&mut server, "{not json");
        assert_eq!(reply["error"]["code"], PARSE_ERROR);
        assert_eq!(reply["id"], Value::Null);
        for line in [
            "[]",
            "42",
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":{"x":1},"method":"initialize"}"#,
        ] {
            let reply = one(&mut server, line);
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
            assert_eq!(reply["id"], Value::Null);
        }
    }

    #[test]
    fn serve_writes_one_flushed_line_per_reply_and_ends_at_end_of_input() {
        let input =
            format!("{INITIALIZE}\n\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}}\r\n");
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        for line in lines {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }

    #[test]
    fn serve_survives_a_last_line_without_a_newline_and_invalid_utf8() {
        let mut input = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(INITIALIZE.as_bytes());
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let replies: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(replies[1]["result"]["protocolVersion"], 1);
    }

    #[test]
    fn a_string_with_a_newline_stays_on_one_line() {
        let mut server = Server::new();
        // The method name is echoed in the error message.
        let reply = one(&mut server, r#"{"jsonrpc":"2.0","id":1,"method":"a\nb"}"#);
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains('\n'));
    }
}
