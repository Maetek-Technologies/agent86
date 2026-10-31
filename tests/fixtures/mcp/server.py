"""A stdlib-only MCP server, for agent86's MCP client tests.

The first argument picks a behaviour:
  normal  - serve over stdio (the server pings the client before answering tools/list)
  crash   - write to stderr and exit before answering initialize
  hang    - read requests and never answer
  http    - serve streamable HTTP on 127.0.0.1; prints "PORT <n>" first
  sse     - serve the legacy HTTP+SSE transport on 127.0.0.1; prints "PORT <n>" first
  shapes  - serve over stdio with one tool, `shapes`, that reports the JSON type of each argument
"""

import json
import os
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = sys.argv[1] if len(sys.argv) > 1 else "normal"

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the text back.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "Add two numbers.",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {"name": "secret", "inputSchema": {"type": "object"}},
    {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
    {"name": "media", "description": "Non-text content.", "inputSchema": {"type": "object"}},
    {"name": "slow", "description": "Sleeps.", "inputSchema": {"type": "object"}},
    {"name": "big", "description": "A long result.", "inputSchema": {"type": "object"}},
    {"name": "bad.name/with spaces", "description": "", "inputSchema": {}},
    {"name": "die", "description": "Exits the server.", "inputSchema": {"type": "object"}},
    {"name": "change", "description": "Changes the tool list.", "inputSchema": {"type": "object"}},
    {"name": "progress", "description": "Reports progress.", "inputSchema": {"type": "object"}},
    {"name": "headers", "description": "Shows request headers.", "inputSchema": {"type": "object"}},
]
if MODE == "shapes":
    TOOLS = [{
        "name": "shapes",
        "description": "Report the JSON type of each argument.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "formats": {"type": "array", "items": {"type": "string"}},
                "options": {"type": "object"},
                "text": {"type": "string"},
            },
        },
    }]
ADDED = {"name": "added", "description": "Appeared later.", "inputSchema": {"type": "object"}}

STATE = {"changed": False, "headers": {}}


def tools():
    if not STATE["changed"]:
        return TOOLS
    return [tool for tool in TOOLS if tool["name"] != "big"] + [ADDED]


def text(value, is_error=False):
    return {"content": [{"type": "text", "text": value}], "isError": is_error}


def call(name, arguments, meta, emit):
    if name == "echo":
        return text(arguments["text"])
    if name == "add":
        return text(str(arguments["a"] + arguments["b"]))
    if name == "shapes":
        kinds = {list: "array", dict: "object", str: "string"}
        return text(",".join(f"{key}={kinds.get(type(value), 'other')}"
                             for key, value in sorted(arguments.items())))
    if name == "secret":
        return text("token=" + os.environ.get("MCP_TEST_TOKEN", "<unset>")
                    + " leaked=" + os.environ.get("AGENT86_TEST_LEAK", "<unset>"))
    if name == "fail":
        return text("it broke", is_error=True)
    if name == "media":
        return {"content": [
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a.txt", "mimeType": "text/plain",
                                              "text": "inside"}},
            {"type": "resource_link", "uri": "file:///b", "name": "b"},
        ]}
    if name == "slow":
        time.sleep(5)
        return text("late")
    if name == "big":
        return text("x" * 350)
    if name == "die":
        sys.stderr.write("dying on request\n")
        sys.stderr.flush()
        os._exit(3)
    if name == "change":
        STATE["changed"] = True
        emit({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        return text("changed")
    if name == "progress":
        token = (meta or {}).get("progressToken")
        for step, message in ((1, "half way"), (2, "done")):
            emit({"jsonrpc": "2.0", "method": "notifications/progress",
                  "params": {"progressToken": token, "progress": step, "total": 2,
                             "message": message}})
        return text("progressed token=" + json.dumps(token))
    if name == "headers":
        return text(json.dumps(STATE["headers"], sort_keys=True))
    if name == "added":
        return text("new tool")
    return None


def handle(request, emit):
    """The response to one request (None for a notification)."""
    if "id" not in request:
        return None
    method = request.get("method")
    params = request.get("params") or {}

    def ok(value):
        return {"jsonrpc": "2.0", "id": request["id"], "result": value}

    def error(code, message):
        return {"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": code, "message": message}}

    if method == "initialize":
        return ok({
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {"listChanged": True}, "resources": {"subscribe": True},
                             "prompts": {}, "completions": {}},
            "serverInfo": {"name": "test-server", "version": "1"},
            "instructions": "Prefer echo for greetings.\n\nNever call slow.",
        })
    if method == "tools/list":
        return ok({"tools": tools()})
    if method == "tools/call":
        value = call(params["name"], params.get("arguments") or {}, params.get("_meta"), emit)
        if value is None:
            return error(-32602, "Unknown tool: " + params["name"])
        return ok(value)
    if method == "resources/list":
        return ok({"resources": [
            {"uri": "file:///notes.txt", "name": "notes", "mimeType": "text/plain",
             "description": "Some  notes\nhere"},
            {"uri": "file:///logo.png", "name": "logo"},
        ]})
    if method == "resources/templates/list":
        return ok({"resourceTemplates": [{"uriTemplate": "file:///{name}", "name": "file"}]})
    if method == "resources/read":
        if params["uri"] == "file:///logo.png":
            return ok({"contents": [{"uri": params["uri"], "mimeType": "image/png",
                                     "blob": "AAAA"}]})
        return ok({"contents": [{"uri": params["uri"], "text": "note text"}]})
    if method in ("resources/subscribe", "resources/unsubscribe"):
        return ok({})
    if method == "prompts/list":
        return ok({"prompts": [{"name": "greet", "description": "Say hello.",
                                "arguments": [{"name": "who", "required": True},
                                              {"name": "tone"}]}]})
    if method == "prompts/get":
        who = (params.get("arguments") or {}).get("who", "nobody")
        return ok({"description": "A greeting.", "messages": [
            {"role": "user", "content": {"type": "text", "text": "Greet " + who}},
            {"role": "assistant", "content": {"type": "image", "data": "", "mimeType": "image/png"}},
        ]})
    if method == "completion/complete":
        ref = params["ref"]
        values = ["alice", "albert"] if ref.get("type") == "ref/prompt" else ["notes.txt"]
        prefix = params["argument"]["value"]
        values = [v for v in values if v.startswith(prefix)]
        return ok({"completion": {"values": values, "total": len(values), "hasMore": False}})
    return error(-32601, "Method not found")


# --------------------------------------------------------------------------- stdio

def serve_stdio():
    def send(message):
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    def read():
        line = sys.stdin.readline()
        if not line:
            sys.exit(0)
        return json.loads(line)

    if MODE == "crash":
        sys.stderr.write("Traceback (most recent call last):\n")
        sys.stderr.write("ModuleNotFoundError: No module named 'imaginary_server'\n")
        sys.stderr.flush()
        sys.exit(1)
    pinged = False
    while True:
        request = read()
        if MODE == "hang":
            continue
        if request.get("method") == "tools/list" and not pinged:
            # Ping the client first; it must answer before we go on.
            pinged = True
            send({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"})
            reply = read()
            assert reply.get("id") == "ping-1" and "result" in reply, reply
        response = handle(request, send)
        if response is not None:
            send(response)


# --------------------------------------------------------------------------- HTTP

STREAM = queue.Queue()  # server-initiated messages for the open GET / SSE stream
SESSION = "sess-1"


def log(line):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"null")

    def reply(self, status, payload=None, content_type="application/json", headers=None):
        data = b"" if payload is None else json.dumps(payload).encode()
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        if payload is not None:
            self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def event_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

    def write_event(self, message, event="message"):
        data = message if isinstance(message, str) else json.dumps(message)
        self.wfile.write(f"event: {event}\ndata: {data}\n\n".encode())
        self.wfile.flush()

    def pump(self):
        while True:
            try:
                message = STREAM.get(timeout=0.2)
            except queue.Empty:
                continue
            try:
                self.write_event(message)
            except OSError:
                return

    # -- streamable HTTP --

    def do_POST(self):
        if MODE == "sse":
            return self.sse_post()
        request = self.body()
        STATE["headers"] = {
            name: self.headers.get(name)
            for name in ("authorization", "mcp-protocol-version", "mcp-session-id")
        }
        is_init = request.get("method") == "initialize"
        if not is_init and self.headers.get("mcp-session-id") != SESSION:
            return self.reply(400, {"error": "missing session"})
        if "id" not in request:
            handle(request, STREAM.put)
            return self.reply(202)
        if request.get("method") == "tools/call" and request["params"]["name"] == "progress":
            # Answer as an SSE stream: the progress notifications, then the result.
            self.event_stream()
            sent = []
            response = handle(request, sent.append)
            for message in sent + [response]:
                self.write_event(message)
            self.close_connection = True
            return
        response = handle(request, STREAM.put)
        headers = {"Mcp-Session-Id": SESSION} if is_init else {}
        self.reply(200, response, headers=headers)

    def do_GET(self):
        if MODE == "sse":
            return self.sse_get()
        if self.headers.get("mcp-session-id") != SESSION:
            return self.reply(400, {"error": "missing session"})
        self.event_stream()
        self.pump()

    def do_DELETE(self):
        log("DELETE " + str(self.headers.get("mcp-session-id")))
        self.reply(200)

    # -- legacy SSE --

    def sse_get(self):
        self.event_stream()
        self.write_event("/messages?session_id=abc", event="endpoint")
        self.pump()

    def sse_post(self):
        if not self.path.startswith("/messages?session_id=abc"):
            return self.reply(404, {"error": "unknown endpoint"})
        request = self.body()
        self.reply(202)
        response = handle(request, STREAM.put)
        if response is not None:
            STREAM.put(response)


def serve_http():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    log(f"PORT {server.server_address[1]}")
    server.serve_forever()


if MODE in ("http", "sse"):
    serve_http()
else:
    serve_stdio()
