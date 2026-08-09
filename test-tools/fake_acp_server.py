#!/usr/bin/env python3
"""Fake ACP server for testing sigil's ACP client.

Speaks just enough of the protocol to exercise `AcpClient`: answers
agent/initialize, agent/session/new, and agent/session.prompt with a
fixed text reply, then agent/session.close.
"""
import json
import sys


def respond(id_, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id_}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    print(json.dumps(msg), flush=True)


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
        id_ = msg.get("id")
        if method == "agent/initialize":
            respond(id_, {
                "protocolVersion": "2025-03-26",
                "agentCapabilities": {},
            })
        elif method == "agent/session/new":
            respond(id_, {"sessionId": "fake-session-1"})
        elif method == "agent/session.prompt":
            # Stream a text update, then complete the turn.
            update = {
                "jsonrpc": "2.0",
                "method": "client/session.update",
                "params": {
                    "sessionId": "fake-session-1",
                    "update": {
                        "message": {
                            "role": "agent",
                            "content": [{"type": "text", "text": "Hello from fake ACP agent"}],
                        }
                    },
                },
            }
            print(json.dumps(update), flush=True)
            respond(id_, {"stopReason": "turn_complete"})
        elif method == "agent/session.close":
            respond(id_, {})


if __name__ == "__main__":
    main()
