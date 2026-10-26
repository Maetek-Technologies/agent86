"""A stdlib-only MCP server over stdio, for agent86's MCP client tests.

The first argument picks a behaviour:
  normal  - serve the tools below (tools/list is paginated, and the server pings
            the client before answering the second page)
  crash   - write to stderr and exit before answering initialize
  hang    - read requests and never answer
"""

import json
import os
import sys
import time

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
]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def result(request, value):
    send({"jsonrpc": "2.0", "id": request["id"], "result": value})


def text(value, is_error=False):
    return {"content": [{"type": "text", "text": value}], "isError": is_error}


def call(name, arguments):
    if name == "echo":
        return text(arguments["text"])
    if name == "add":
        return text(str(arguments["a"] + arguments["b"]))
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
    return None


def main():
    if MODE == "crash":
        sys.stderr.write("Traceback (most recent call last):\n")
        sys.stderr.write("ModuleNotFoundError: No module named 'imaginary_server'\n")
        sys.stderr.flush()
        sys.exit(1)
    while True:
        request = read()
        if MODE == "hang":
            continue
        method = request.get("method")
        if "id" not in request:
            continue  # a notification
        if method == "initialize":
            result(request, {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "test-server", "version": "1"},
                "instructions": "Prefer echo for greetings.\n\nNever call slow.",
            })
        elif method == "tools/list":
            cursor = request.get("params", {}).get("cursor")
            if cursor is None:
                result(request, {"tools": TOOLS[:3], "nextCursor": "page2"})
            else:
                # Ping the client mid-listing; it must answer before we go on.
                send({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"})
                reply = read()
                assert reply.get("id") == "ping-1" and "result" in reply, reply
                result(request, {"tools": TOOLS[3:]})
        elif method == "tools/call":
            params = request["params"]
            value = call(params["name"], params.get("arguments") or {})
            if value is None:
                send({"jsonrpc": "2.0", "id": request["id"],
                      "error": {"code": -32602, "message": "Unknown tool: " + params["name"]}})
            else:
                result(request, value)
        else:
            send({"jsonrpc": "2.0", "id": request["id"],
                  "error": {"code": -32601, "message": "Method not found"}})


main()
