#!/usr/bin/env python3
"""Minimal MCP stdio server for sigil tests: one echo tool.

Speaks JSON-RPC 2.0 over NDJSON (one JSON object per line) on
stdin/stdout, exactly like the official MCP Python SDK does.
"""
import json
import sys


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method", "")
        rid = msg.get("id")

        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "echo-test", "version": "0.0.1"},
                },
            })
        elif method == "notifications/initialized":
            pass  # no response needed
        elif method == "prompts/list":
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "prompts": [{
                        "name": "greet",
                        "description": "Render a greeting prompt",
                        "arguments": [{
                            "name": "who",
                            "description": "Who to greet",
                            "required": False,
                        }],
                    }]
                },
            })
        elif method == "prompts/get":
            name = msg.get("params", {}).get("name", "")
            if name == "greet":
                send({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {
                        "description": "Render a greeting prompt",
                        "messages": [{
                            "role": "user",
                            "content": {
                                "type": "text",
                                "text": "Please greet the user warmly.",
                            },
                        }],
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32602, "message": f"prompt not found: {name}"},
                })
        elif method == "resources/list":
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "resources": [{
                        "uri": "echo://greeting",
                        "name": "greeting",
                        "description": "A static greeting resource",
                        "mimeType": "text/markdown",
                    }]
                },
            })
        elif method == "resources/read":
            uri = msg.get("params", {}).get("uri", "")
            if uri == "echo://greeting":
                send({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {
                        "contents": [{
                            "uri": uri,
                            "mimeType": "text/markdown",
                            "text": "# Greeting\n\nHello from the echo server resource!",
                        }]
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32002, "message": f"resource not found: {uri}"},
                })
        elif method == "tools/list":
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo back the text argument",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }]
                },
            })
        elif method == "tools/call":
            params = msg.get("params", {})
            args = params.get("arguments", {})
            text = args.get("text", "")
            send({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "content": [{"type": "text", "text": f"echo: {text}"}],
                },
            })
        elif method == "ping":
            send({"jsonrpc": "2.0", "id": rid, "result": {}})
        else:
            if rid is not None:
                send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"unknown method {method}"}})


if __name__ == "__main__":
    main()
