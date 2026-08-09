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
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-test", "version": "0.0.1"},
                },
            })
        elif method == "notifications/initialized":
            pass  # no response needed
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
