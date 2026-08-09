#!/usr/bin/env python3
"""MCP client E2E test: drive `sigil mcp` like a real MCP client.

Speaks JSON-RPC 2.0 over NDJSON on stdio (same framing as Claude
Desktop / Cursor). Verifies:
  1. initialize → serverInfo.name == "sigil"
  2. notifications/initialized
  3. tools/list → contains bash, read_file, write_file, delegate, ...
  4. tools/call bash {"command":"echo hello-from-mcp"} → "hello-from-mcp"
  5. ping → empty result
"""
import json
import subprocess
import sys

EXE = "/mnt/d/wsl2/sigil/target/release/sigil"


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def recv(proc, timeout=30):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("server closed stdout")
    return json.loads(line)


def main():
    proc = subprocess.Popen(
        [EXE, "mcp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, bufsize=1,
    )

    # 1. initialize
    send(proc, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                   "clientInfo": {"name": "e2e-client", "version": "1.0"}},
    })
    resp = recv(proc)
    assert resp["id"] == 1, resp
    info = resp["result"]["serverInfo"]
    assert info["name"] == "sigil", resp
    print(f"1. initialize OK: serverInfo={info['name']} v{info['version']}")

    # 2. initialized notification (no response expected)
    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

    # 3. tools/list
    send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    resp = recv(proc)
    tools = {t["name"] for t in resp["result"]["tools"]}
    assert "bash" in tools and "read_file" in tools and "write_file" in tools, tools
    assert "delegate" in tools, tools
    print(f"2. tools/list OK: {len(tools)} tools (bash, read_file, delegate, ...)")

    # 4. tools/call bash echo
    send(proc, {
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "bash", "arguments": {"command": "echo hello-from-mcp"}},
    })
    resp = recv(proc)
    text = resp["result"]["content"][0]["text"].strip()
    assert text == "hello-from-mcp", resp
    print(f"3. tools/call OK: bash → '{text}'")

    # 5. ping
    send(proc, {"jsonrpc": "2.0", "id": 4, "method": "ping", "params": {}})
    resp = recv(proc)
    assert resp["id"] == 4 and "result" in resp, resp
    print("4. ping OK")

    # Close: EOF on stdin.
    proc.stdin.close()
    proc.wait(timeout=10)
    print("== MCP SERVER E2E PASS ==")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"FAIL: {e}", file=sys.stderr)
        sys.exit(1)
