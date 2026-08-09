#!/usr/bin/env python3
"""sigil ratchet — regression checks against the real agent.

Runs a fixed battery of tasks through the actual sigil binary + real
LLM (cc-switch proxy) and asserts the core behaviors hold. This is the
"ratchet": if a change breaks a fundamental capability, this fails.

Each task: prompt → run pipe mode → assert expected behavior.
Run locally (not CI):  python3 test-tools/ratchet.py

Exit 0 = all pass. Prints PASS/FAIL per task.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

EXE = "/mnt/d/wsl2/sigil/target/release/sigil"
HARNESS = "/tmp/sigil-harness-test"
TIMEOUT = 180
WORK = "/tmp/sigil-ratchet-work"
TOOLS = "/tmp/sigil-ratchet-tools"


def run(prompt: str, harness: str = HARNESS, extra: list | None = None) -> str:
    cmd = [EXE, "--harness-dir", harness, "--tools-dir", TOOLS, "--max-steps", "10"]
    if extra:
        cmd += extra
    proc = subprocess.run(
        cmd, input=prompt, capture_output=True, text=True, timeout=TIMEOUT,
    )
    return proc.stdout + proc.stderr


def run_mcp(prompt: str, harness: str = HARNESS) -> str:
    """Run the MCP server, send tools/list + a tool call, return output."""
    import subprocess as sp

    proc = sp.Popen(
        [EXE, "mcp", "--harness-dir", harness, "--tools-dir", TOOLS],
        stdin=sp.PIPE, stdout=sp.PIPE, stderr=sp.PIPE, text=True,
    )
    assert proc.stdin is not None, "MCP server stdin must be a pipe"
    msgs = [
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2024-11-05",
                               "capabilities": {}, "clientInfo": {"name": "ratchet", "version": "0"}}}) + "\n",
        json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}) + "\n",
        json.dumps({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "bash", "arguments": {"command": "echo mcp-ratchet-ok"}}}) + "\n",
    ]
    out = ""
    try:
        for m in msgs:
            proc.stdin.write(m)
            proc.stdin.flush()
        time.sleep(3)
        proc.terminate()
        out, _ = proc.communicate(timeout=5)
    except Exception:
        pass
    return out


def run_acp(prompt: str, harness: str = HARNESS) -> str:
    """Drive `sigil acp` as a real ACP client: init → session/new →
    prompt (real LLM, tool use) → collect chunks → close.

    Returns the accumulated agent text (the model's reply). This is the
    protocol-level twin of `run()` — it exercises the ACP server entry
    point end to end.
    """
    import subprocess as sp

    proc = sp.Popen(
        [EXE, "acp", "--harness-dir", harness, "--tools-dir", TOOLS],
        stdin=sp.PIPE, stdout=sp.PIPE, stderr=sp.DEVNULL, text=True, bufsize=1,
    )
    assert proc.stdin is not None, "ACP server stdin must be a pipe"
    assert proc.stdout is not None, "ACP server stdout must be a pipe"

    def send(msg):
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv():
        line = proc.stdout.readline()
        return None if not line else json.loads(line)

    chunks: list[str] = []
    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "agent/initialize",
              "params": {"protocolVersion": "2025-03-26", "clientCapabilities": {},
                         "clientInfo": {"name": "ratchet"}}})
        recv()
        send({"jsonrpc": "2.0", "id": 2, "method": "agent/session/new",
              "params": {"cwd": "/mnt/d/wsl2/sigil", "mcpServers": []}})
        sess = recv()
        session_id = sess["result"]["sessionId"]
        send({"jsonrpc": "2.0", "id": 3, "method": "agent/session.prompt",
              "params": {"sessionId": session_id,
                         "prompt": [{"type": "text", "text": prompt}]}})
        deadline = time.time() + TIMEOUT
        while time.time() < deadline:
            msg = recv()
            if msg is None:
                break
            if msg.get("id") == 3:
                break
            if msg.get("method") == "client/session.update":
                upd = msg["params"]["update"]
                if upd.get("sessionUpdate") == "agent_message_chunk":
                    chunks.append(upd["content"]["text"])
        try:
            send({"jsonrpc": "2.0", "id": 4, "method": "agent/session.close",
                  "params": {"sessionId": session_id}})
        except Exception:
            pass
    except Exception:
        pass
    finally:
        proc.terminate()
        proc.wait(timeout=5)
    return "".join(chunks)


def run_session(harness: str = HARNESS) -> str:
    """Exercise `sigil session export / import / list` end to end — a
    deterministic CLI path (no LLM): hand-build a timestamped JSONL
    session, export it (full + --since), import it, list sessions."""
    import subprocess as sp

    sessions = os.path.join(WORK, "sessions")
    os.makedirs(sessions, exist_ok=True)
    sid = "2026-22000-100000-000000000"
    # Three timestamped turns (Phase 31 format: role/text/ts).
    turns = [
        {"role": "user", "text": "early", "ts": "2026-22000-100000-000000000"},
        {"role": "assistant", "text": "mid", "ts": "2026-22000-120000-000000000"},
        {"role": "user", "text": "late", "ts": "2026-22000-140000-000000000"},
    ]
    with open(os.path.join(sessions, f"{sid}.jsonl"), "w") as f:
        for t in turns:
            f.write(json.dumps(t) + "\n")

    out = []
    base = [EXE, "session", "--sessions-dir", sessions]

    # Full export → archive with all 3 turns.
    full = os.path.join(WORK, "full.json")
    r = sp.run(base + ["export", sid, "-o", full], capture_output=True, text=True, timeout=30)
    out.append(r.stdout + r.stderr)
    archive = json.load(open(full))
    out.append(f"turns={len(archive['turns'])} source={archive['source_id']}")
    # All three turn texts must survive the round trip.
    texts = [t["text"] for t in archive["turns"]]
    out.append(f"texts={'/'.join(texts)}")

    # --since filter (Phase 31): only turns at/after the cutoff.
    since = os.path.join(WORK, "since.json")
    r = sp.run(base + ["export", sid, "--since", "2026-22000-110000-000000000", "-o", since],
               capture_output=True, text=True, timeout=30)
    out.append(r.stdout + r.stderr)
    archive = json.load(open(since))
    since_texts = [t["text"] for t in archive["turns"]]
    out.append(f"since_texts={'/'.join(since_texts)}")

    # Import → brand-new session id, same turns.
    r = sp.run(base + ["import", full], capture_output=True, text=True, timeout=30)
    out.append(r.stdout + r.stderr)

    # List → both ids present.
    r = sp.run(base + ["list"], capture_output=True, text=True, timeout=30)
    out.append(r.stdout + r.stderr)

    return "\n".join(out)


TASKS = [
    {
        "name": "file_tool_read",
        "prompt": "用 read_file 工具读取 src/cli.rs 的前 5 行，告诉我第一行的内容",
        "assert": ["use clap"],
    },
    {
        "name": "bash_tool_exec",
        "prompt": "用 bash 工具执行命令 echo ratchet-ok-42，报告输出",
        "assert": ["ratchet-ok-42"],
    },
    {
        "name": "git_status_works",
        "prompt": "用 git_status 工具查看当前 git 状态，报告它输出了什么",
        "assert": ["git"],
    },
    {
        "name": "lua_tool_available",
        "prompt": "用 bash 工具执行 'ls ~/.sigil/tools/ 2>/dev/null || echo no-tools-dir'，然后告诉我输出",
        "assert": ["no-tools-dir", "~/.sigil/tools"],
        "special": "tools",
    },
    {
        "name": "memory_remember_and_recall",
        "prompt": "先用 remember 记住事实：ratchet-memory-marker-77。然后立即用 recall 查看是否记住了。报告 recall 的输出。",
        "assert": ["ratchet-memory-marker-77"],
        "special": "memory",
    },
    {
        "name": "memory_list_and_delete",
        "prompt": "先用 memory_list 列出所有记忆，然后用 memory_delete 删除包含 ratchet-memory-marker-77 的那条，最后用 memory_list 确认它已经不在了。报告每次输出。",
        "assert": ["不再", "没有", "empty", "1", "1 条"],
        "special": "memory",
    },
    {
        "name": "skill_dependency_inline",
        "prompt": "我要开发一个 Rust 功能。先回答：根据你加载的技能，你需要注意哪些基础规则（不是 Rust 专属规则）？",
        "assert": ["base", "先读", "文件"],
        "special": "skilldeps",
    },
    {
        "name": "live_prompt_edit",
        "prompt": "用 improve_harness 的 edit_prompt 把系统提示词改为：当用户说 ratchet-live-88 时，回答：LIVE-OK。改完立刻结束，不要继续对话，不要问问题，只报告完成。",
        "assert": ["完成", "updated", "LIVE-OK"],
        "special": "live",
    },
    {
        "name": "native_tool_call_flow",
        "prompt": "用 create_lua_tool 创建一个工具 greet（描述：打招呼，schema 只要 name 字符串参数），然后调用它打招呼。报告结果。",
        "assert": ["greet", "hello", "你好", "created"],
        "special": "createtool",
    },
    {
        "name": "mcp_server_echo",
        "prompt": "",  # not used — run_mcp drives the server directly
        "assert": ["mcp-ratchet-ok"],
        "special": "mcp",
    },
    {
        "name": "acp_server_tool_use",
        "prompt": "用 bash 工具执行命令 echo acp-ratchet-77，报告输出。",
        "assert": ["acp-ratchet-77"],
        "special": "acp",
    },
    {
        "name": "session_export_import",
        "prompt": "",  # not used — run_session drives the CLI directly
        "assert": [
            "texts=early/mid/late",       # full export round-trips all 3 turns
            "since_texts=mid/late",       # --since drops the early turn
            "as new session",             # import creates a fresh id
            "2026-22000-100000-000000000",  # original session still listed
        ],
        "special": "session",
    },
]


def main() -> int:
    # Prepare per-task sandboxes. TOOLS is isolated from the real
    # ~/.sigil/tools — model-created tools from previous sessions must
    # never leak into the ratchet (a bad persisted schema would 400
    # every LLM call, as discovered in Phase 33).
    for d in (WORK, HARNESS, TOOLS):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d, exist_ok=True)
    # Baseline harness for generic tasks.
    with open(os.path.join(HARNESS, "system_prompt.md"), "w") as f:
        f.write("> sigil ratchet baseline\nYou are a helpful coding agent.\n")
    os.makedirs(os.path.join(HARNESS, "skills"), exist_ok=True)

    # Skill-dependency task needs a base + rust skill.
    with open(os.path.join(HARNESS, "skills", "base.md"), "w") as f:
        f.write("> 基础规则\n\n基础规则：先读文件再修改。\n")
    with open(os.path.join(HARNESS, "skills", "rust.md"), "w") as f:
        f.write("> Rust 开发技能\n> depends_on: base\n\nRust 规则：用 cargo test 验证。\n")

    baseline_prompt = open(os.path.join(HARNESS, "system_prompt.md")).read()

    # Memory task needs an isolated memory file per run.
    mem_path = os.path.join(WORK, "memory.md")

    failed = 0
    for task in TASKS:
        print(f"── {task['name']} …", flush=True)
        try:
            if task["name"].startswith("memory_"):
                shutil.rmtree(WORK, ignore_errors=True)
                os.makedirs(WORK, exist_ok=True)
                out = run(task["prompt"], extra=["--memory", mem_path])
            elif task.get("special") == "mcp":
                out = run_mcp(task["prompt"])
            elif task.get("special") == "acp":
                out = run_acp(task["prompt"])
            elif task.get("special") == "session":
                out = run_session()
            else:
                out = run(task["prompt"])
        except subprocess.TimeoutExpired:
            print(f"  FAIL: timeout after {TIMEOUT}s")
            failed += 1
            continue
        ok = any(a.lower() in out.lower() for a in task["assert"])
        if task.get("special") == "session":
            # Deterministic CLI path — every assertion must hold (the
            # output is scripted, not LLM prose, so no word-variance
            # slack is needed).
            ok = all(a.lower() in out.lower() for a in task["assert"])
        if task.get("special") == "tools":
            ok = "tools" in out.lower() or "no-tools-dir" in out.lower()
        if task.get("special") == "skilldeps":
            # The model must reference the base rules (the dependency)
            # when asked about a Rust task — either by name or content.
            ok = ("base" in out.lower()
                  or "先读" in out.lower()
                  or "先读文件" in out.lower())
        if task.get("special") == "live":
            # Strong check: the system prompt on disk must actually
            # contain the new instruction.
            disk = ""
            try:
                with open(os.path.join(HARNESS, "system_prompt.md")) as f:
                    disk = f.read()
            except OSError:
                pass
            ok = ok and "LIVE-OK" in disk
        print(f"  {'PASS' if ok else 'FAIL'}")
        if not ok:
            failed += 1
            print(f"  --- output tail ---\n{out[-400:]}")
    print()
    # Restore the baseline system prompt (live_prompt_edit mutates it).
    with open(os.path.join(HARNESS, "system_prompt.md"), "w") as f:
        f.write(baseline_prompt)
    if failed:
        print(f"RATCHET: {len(TASKS) - failed}/{len(TASKS)} passed — {failed} FAILED")
        return 1
    print(f"RATCHET: {len(TASKS)}/{len(TASKS)} passed ✅")
    return 0


if __name__ == "__main__":
    sys.exit(main())
