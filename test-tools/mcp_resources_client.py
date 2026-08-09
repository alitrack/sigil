#!/usr/bin/env python3
"""E2E: drive `sigil mcp`, assert resources/list and resources/read."""
import json
import select
import subprocess

proc = subprocess.Popen(
    ["./target/release/sigil", "--harness-dir", "/tmp/sigil-dyn", "mcp"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
)


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def recv(timeout=10):
    r, _, _ = select.select([proc.stdout], [], [], timeout)
    if not r:
        return None
    line = proc.stdout.readline()
    return json.loads(line) if line.strip() else None


# Handshake
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
init = recv()
caps = init["result"]["capabilities"]
assert "resources" in caps, f"missing resources capability: {caps}"
print("✓ initialize — capabilities:", sorted(caps.keys()))

send({"jsonrpc": "2.0", "method": "notifications/initialized"})

# resources/list → expect system-prompt + math + git skills
send({"jsonrpc": "2.0", "id": 2, "method": "resources/list"})
listing = recv()
resources = listing["result"]["resources"]
uris = [r["uri"] for r in resources]
print(f"✓ resources/list — {len(resources)} resources: {uris}")
assert "sigil://system-prompt" in uris
assert "sigil://skills/math" in uris
assert "sigil://skills/git" in uris

# resources/read a skill → content must match the file body
send({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
      "params": {"uri": "sigil://skills/math"}})
read = recv()
text = read["result"]["contents"][0]["text"]
assert "数学技能" in text, f"skill content mismatch: {text[:60]}"
print("✓ resources/read — sigil://skills/math content OK")

# resources/read system prompt
send({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
      "params": {"uri": "sigil://system-prompt"}})
read = recv()
assert "sigil" in read["result"]["contents"][0]["text"].lower()
print("✓ resources/read — sigil://system-prompt content OK")

# resources/read unknown → error
send({"jsonrpc": "2.0", "id": 5, "method": "resources/read",
      "params": {"uri": "sigil://nope"}})
err = recv()
assert err["error"]["code"] == -32002, f"expected -32002, got {err}"
print("✓ resources/read — unknown uri returns -32002")

proc.terminate()
print("E2E MCP resources PASSED")
