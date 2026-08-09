#!/usr/bin/env python3
"""Drive sigil acp like a real ACP client: init → session → prompt → close."""
import json
import subprocess
import sys
import time

EXE = "/mnt/d/wsl2/sigil/target/release/sigil"


def send(proc, msg):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def recv(proc, timeout=120):
    line = proc.stdout.readline()
    if not line:
        return None
    return json.loads(line)


def main():
    proc = subprocess.Popen(
        [EXE, "acp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, bufsize=1,
    )

    # 1. initialize
    send(proc, {"jsonrpc": "2.0", "id": 1, "method": "agent/initialize",
                "params": {"protocolVersion": "2025-03-26", "clientCapabilities": {},
                           "clientInfo": {"name": "e2e-client"}}})
    init = recv(proc)
    assert init["id"] == 1, init
    print("initialize OK:", init["result"]["protocolVersion"])

    # 2. session/new
    send(proc, {"jsonrpc": "2.0", "id": 2, "method": "agent/session/new",
                "params": {"cwd": "/mnt/d/wsl2/sigil", "mcpServers": []}})
    sess = recv(proc)
    session_id = sess["result"]["sessionId"]
    print("session/new OK:", session_id[:20], "...")

    # 3. session.prompt — real LLM, tool use
    send(proc, {"jsonrpc": "2.0", "id": 3, "method": "agent/session.prompt",
                "params": {"sessionId": session_id,
                           "prompt": [{"type": "text", "text": "用 git_status 工具查看当前仓库状态，简要报告"}]}})

    # 4. Collect notifications until the response arrives
    chunks = []
    response = None
    deadline = time.time() + 180
    while time.time() < deadline:
        msg = recv(proc, timeout=180)
        if msg is None:
            print("ERROR: connection closed")
            sys.exit(1)
        if msg.get("id") == 3:
            response = msg
            break
        if msg.get("method") == "client/session.update":
            upd = msg["params"]["update"]
            if upd.get("sessionUpdate") == "agent_message_chunk":
                chunks.append(upd["content"]["text"])
    assert response is not None, "no prompt response"
    print("prompt response OK: stopReason =", response["result"]["stopReason"])
    print("--- agent text ---")
    print("".join(chunks))
    print("------------------")

    # 5. session/close
    send(proc, {"jsonrpc": "2.0", "id": 4, "method": "agent/session.close",
                "params": {"sessionId": session_id}})
    close = recv(proc)
    assert close["id"] == 4, close
    print("session/close OK")

    proc.terminate()
    proc.wait(timeout=10)
    print("== ACP E2E PASS ==")


if __name__ == "__main__":
    main()
