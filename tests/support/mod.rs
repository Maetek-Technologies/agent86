//! A minimal scripted HTTP/1.1 server for provider integration tests.
//!
//! Each accepted connection consumes the next [`Reply`] in order, so a test
//! can script retries, failures, and successes without network access.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How the server answers one connection.
pub enum Reply {
    /// A complete response with a `Content-Length` body.
    Status {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// A 200 SSE response whose body ends when the server closes the connection.
    Sse(String),
    /// A 200 chunked SSE response that is cut off before its terminating chunk.
    Truncated(String),
    /// A 200 SSE response that sends `prefix` and then stops sending without closing.
    StallAfter(String),
    /// A complete response with exactly these headers (no default content type).
    Raw {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
}

/// One request the server received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub request_line: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub struct MockServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let mut replies = VecDeque::from(replies);

        tokio::spawn(async move {
            while let Some(reply) = replies.pop_front() {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move { serve(socket, reply, recorded).await });
            }
        });

        Self { base_url, requests }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// An OpenAI-style SSE reply that streams `text` and ends with `finish_reason`.
pub fn sse_text(text: &str, finish_reason: &str) -> Reply {
    let chunks = [
        serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]}),
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
    ];
    Reply::Sse(sse_body(&chunks))
}

/// An OpenAI-style SSE reply that reasons (`reasoning_content`, as llama.cpp sends it)
/// and then says `text`, which may be empty, ending with `stop`.
pub fn sse_reasoning(reasoning: &str, text: &str) -> Reply {
    let chunks = [
        serde_json::json!({"choices": [{"index": 0, "delta": {"reasoning_content": reasoning}, "finish_reason": null}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
    ];
    Reply::Sse(sse_body(&chunks))
}

/// An OpenAI-style SSE reply asking for tool calls, each `(id, name, raw arguments)`.
pub fn sse_tool_calls(calls: &[(&str, &str, &str)]) -> Reply {
    let fragments: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })
        })
        .collect();
    let chunks = [
        serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": fragments}, "finish_reason": null}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 8}}),
    ];
    Reply::Sse(sse_body(&chunks))
}

fn sse_body(chunks: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Read a fixture from `tests/fixtures`.
pub fn fixture(path: &str) -> String {
    let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|error| panic!("{full}: {error}"))
}

async fn serve(mut socket: TcpStream, reply: Reply, recorded: Arc<Mutex<Vec<Recorded>>>) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    recorded.lock().unwrap().push(request);

    match reply {
        Reply::Status {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Reply::Raw {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
        Reply::Sse(body) => {
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Reply::Truncated(body) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let chunk = format!("{:x}\r\n{body}\r\n", body.len());
            let _ = socket.write_all(chunk.as_bytes()).await;
            // Close without the terminating zero-length chunk.
        }
        Reply::StallAfter(prefix) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            if !prefix.is_empty() {
                let chunk = format!("{:x}\r\n{prefix}\r\n", prefix.len());
                let _ = socket.write_all(chunk.as_bytes()).await;
            }
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    let _ = socket.shutdown().await;
}

async fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };

    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);

    while data.len() < header_end + length {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    let body = serde_json::from_slice(&data[header_end..]).unwrap_or(serde_json::Value::Null);
    Some(Recorded {
        request_line,
        headers,
        body,
    })
}

/// A tool with an array, an object, and a string parameter that answers with the
/// JSON type each argument arrived as (`formats=array,options=object,text=string`),
/// for the tests of argument repair.
pub struct ShapesTool;

impl agent86::tools::Tool for ShapesTool {
    fn name(&self) -> &str {
        "shapes"
    }

    fn description(&self) -> &str {
        "Report the JSON type of each argument."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "formats": {"type": "array", "items": {"type": "string"}},
                "options": {"anyOf": [{"type": "object"}, {"type": "null"}]},
                "text": {"type": "string"},
            },
        })
    }

    fn execute(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
        _ctx: &agent86::tools::ToolContext,
    ) -> Result<agent86::types::ToolResult, agent86::tools::ToolError> {
        let kinds: Vec<String> = args
            .iter()
            .map(|(key, value)| {
                let kind = match value {
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::Object(_) => "object",
                    serde_json::Value::String(_) => "string",
                    _ => "other",
                };
                format!("{key}={kind}")
            })
            .collect();
        Ok(agent86::tools::success("shapes", kinds.join(",")))
    }
}
