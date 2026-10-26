//! A scripted local HTTP server standing in for an OpenAI-compatible endpoint.
//!
//! Each accepted connection consumes the next scripted reply and records the
//! request it received, so tests can assert on both sides of the exchange
//! without credentials or network access.

#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

/// Read a fixture from `tests/fixtures/openai/`.
pub fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/openai")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[derive(Clone)]
pub enum Reply {
    /// A complete response; the body is delimited by closing the connection.
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// Promise more body bytes than are sent, then close: a broken transfer.
    CutOff { sent: String, promised: usize },
    /// Accept the request and never answer.
    Stall,
}

impl Reply {
    pub fn sse(body: impl Into<String>) -> Self {
        Self::Respond {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: body.into(),
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Respond {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Self::Respond { headers, .. } = &mut self {
            headers.push((name.into(), value.into()));
        }
        self
    }
}

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
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for (stream, reply) in listener.incoming().zip(replies) {
                let Ok(stream) = stream else { break };
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || serve(stream, reply, &recorded));
            }
        });
        Self { address, requests }
    }

    /// The base URL a provider section would name.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// A base URL on which nothing is listening.
pub fn refused_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/v1")
}

fn serve(stream: TcpStream, reply: Reply, recorded: &Mutex<Vec<Recorded>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut headers = Vec::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim().to_owned();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name.trim().to_owned(), value));
        }
    }
    let mut body = vec![0; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    recorded.lock().unwrap().push(Recorded {
        request_line: request_line.trim_end().to_owned(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    });

    let mut stream = stream;
    match reply {
        Reply::Respond {
            status,
            headers,
            body,
        } => {
            let mut head = format!("HTTP/1.1 {status} Mock\r\nConnection: close\r\n");
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
        Reply::CutOff { sent, promised } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: \
                 {promised}\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(sent.as_bytes());
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(50));
        }
        Reply::Stall => {
            thread::sleep(Duration::from_secs(30));
        }
    }
    let _ = stream.flush();
}
