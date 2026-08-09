#!/usr/bin/env python3
"""E2E: drive `sigil mcp`, assert progress notifications around tools/call."""
import json
import subprocess
import sys

proc = subprocess.Popen(
    ["./target/release/sigil", "mcp"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
)


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def recv(timeout=10):
    import select

    r, _, _ = select.select([proc.stdout], [], [], timeout)
    if not r:
        return None
    line = proc.stdout.readline()
    return json.loads(line) if line.strip() else None


# Handshake
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
init = recv()
caps = init["result"]["capabilities"]
assert "progress" in caps, f"missing progress capability: {caps}"
print("✓ initialize — capabilities:", sorted(caps.keys()))

send({"jsonrpc": "2.0", "method": "notifications/initialized"})

send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
listing = recv()
tools = [t["name"] for t in listing["result"]["tools"]]
print(f"✓ tools/list — {len(tools)} tools: {tools[:5]}...")

# tools/call → expect progress(started) → progress(finished) → result
send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
      "params": {"name": "bash", "arguments": {"command": "echo hello"}}})
frames = []
for _ in range(3):
    f = recv()
    if f is None:
        break
    frames.append(f)

progress_frames = [f for f in frames if f.get("method") == "notifications/progress"]
result_frames = [f for f in frames if "id" in f and f.get("id") == 3]
assert len(progress_frames) == 2, f"expected 2 progress notifications, got {len(progress_frames)}"
assert progress_frames[0]["params"]["progress"] == 0.0, "first progress should be 0.0 (started)"
assert progress_frames[1]["params"]["progress"] == 1.0, "second progress should be 1.0 (finished)"
print("✓ progress notifications: started(0.0) → finished(1.0)")
assert len(result_frames) == 1, "expected tools/call result"
text = result_frames[0]["result"]["content"][0]["text"]
assert "hello" in text, f"bash result missing output: {text}"
print(f"✓ tools/call result — bash echo: {text.strip()}")

proc.terminate()
print("E2E MCP progress PASSED")
