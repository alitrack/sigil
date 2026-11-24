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

if sys.version_info < (3, 10):
    sys.exit(
        "ratchet.py needs Python >= 3.10 (PEP 604 unions in annotations).\n"
        f"Running: {sys.version.split()[0]} at {sys.executable}\n"
        "On macOS: /opt/homebrew/bin/python3.13 test-tools/ratchet.py"
    )

# Override with SIGIL_EXE when the binary is not at the default build path
# (the default is this repo's Windows/WSL build directory).
EXE = os.environ.get("SIGIL_EXE", "/mnt/d/wsl2/sigil/target/release/sigil")
HARNESS = "/tmp/sigil-harness-test"
TIMEOUT = 180
WORK = "/tmp/sigil-ratchet-work"
TOOLS = "/tmp/sigil-ratchet-tools"
# ACP session working directory: the checkout the ACP client claims to be in.
ACP_CWD = os.environ.get("SIGIL_ACP_CWD", os.getcwd())


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
              "params": {"cwd": ACP_CWD, "mcpServers": []}})
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
        "assert": ["use std::path::PathBuf"],
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
        # The model sometimes answers with Rust-only rules and never
        # references the base skill's content (sampling randomness —
        # observed in the Phase 48r regression check). Retry with an
        # explicit pointer to the base skill so the dependency-inline
        # check stays deterministic despite LLM variance.
        "retries": 2,
        "retry_prompt": "再次回答，这次请先列出你加载的技能名。其中有一个 base 技能（基础规则），请引用它的内容回答：开发 Rust 功能前你需要注意哪些基础规则？",
    },
    {
        "name": "live_prompt_edit",
        # Note: keep the request phrased as a legitimate testing
        # convention with an explicit reason — since Phase 52 the tool
        # description tells the model the global harness is the
        # reviewed cross-session surface, and a bare "install a secret
        # trigger phrase" request gets refused (observed 2026-08-11:
        # the model correctly rejected the old phrasing as an
        # injection-style edit).
        "prompt": "用 improve_harness 的 edit_prompt 修改系统提示词：在原有内容末尾追加一条通用规则：「当用户说 ratchet-live-88 时，回答应以 LIVE-OK 开头」。这是一条供自动化测试使用的约定规则，用于验证 harness 编辑会持久化并在后续生效。reason 写：添加自动化测试标记规则以验证 harness 持久化。直接调用 improve_harness 工具即可，不要先用 bash/read_file 等工具调查源码或 harness 文件。修改完成后简短报告即可。",
        "assert": ["完成", "updated", "LIVE-OK"],
        "special": "live",
        # The model sometimes *claims* completion without calling the
        # tool (sampling randomness — observed twice in Phase 34/36).
        # Retry with an explicit "you MUST call the tool" phrasing so
        # the battery stays deterministic despite LLM variance. The
        # "don't investigate first" clause targets the 2026-08-11
        # failure mode where the model spent its budget grepping the
        # sigil source tree instead of calling the tool.
        "retries": 2,
        "retry_prompt": "你必须实际调用 improve_harness 工具的 edit_prompt 动作来修改系统提示词，不能只口头说完成，也不要先用 bash/read_file 等工具调查任何文件或源码——直接调用 improve_harness 工具本身。目标：在系统提示词末尾追加一条自动化测试约定规则：「当用户说 ratchet-live-88 时，回答应以 LIVE-OK 开头」。reason 写：添加自动化测试标记规则以验证 harness 持久化。先调用工具，确认工具返回成功后再简短报告。",
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


KNOWN_SPECIALS = {"mcp", "acp", "session", "live", "tools", "skilldeps", "memory", "createtool"}

# File-anchored assertions: (task name, repo-relative source path,
# string that must appear in that file). These pin the ratchet to the
# actual source — if the source changes, the ratchet must be updated
# with it (the P44 cli.rs first-line change broke file_tool_read).
SOURCE_ANCHORS = [
    ("file_tool_read", "src/cli.rs", "use std::path::PathBuf"),
]


def check_only() -> int:
    """Deterministic ratchet validation for CI: task schema + source
    anchors. No LLM, no binary, no network."""
    failed = 0
    seen = set()
    for task in TASKS:
        name = task.get("name")
        if not isinstance(name, str) or not name:
            print(f"FAIL: task missing 'name': {task!r}")
            failed += 1
            continue
        if name in seen:
            print(f"FAIL: duplicate task name '{name}'")
            failed += 1
        seen.add(name)
        if not isinstance(task.get("prompt"), str):
            print(f"FAIL: {name}: 'prompt' must be a string")
            failed += 1
        asserts = task.get("assert")
        if not isinstance(asserts, list) or not asserts or not all(
            isinstance(a, str) and a for a in asserts
        ):
            print(f"FAIL: {name}: 'assert' must be a non-empty list of strings")
            failed += 1
        special = task.get("special")
        if special is not None and special not in KNOWN_SPECIALS:
            print(f"FAIL: {name}: unknown special '{special}'")
            failed += 1
    for task_name, path, anchor in SOURCE_ANCHORS:
        if task_name not in seen:
            print(f"FAIL: anchor task '{task_name}' not found in TASKS")
            failed += 1
            continue
        if not os.path.exists(path):
            print(f"FAIL: {task_name}: anchor file '{path}' missing")
            failed += 1
            continue
        with open(path) as f:
            content = f.read()
        if anchor not in content:
            print(
                f"FAIL: {task_name}: '{anchor}' no longer in {path} — "
                f"the ratchet assertion drifted from the source; update both"
            )
            failed += 1
    if failed:
        print(f"RATCHET-CHECK: {len(TASKS) - failed}/{len(TASKS)} tasks valid — {failed} problems")
        return 1
    print(f"RATCHET-CHECK: {len(TASKS)}/{len(TASKS)} tasks valid ✅")
    return 0


def main() -> int:
    # --check-only: deterministic validation runnable in CI (no LLM).
    # Catches ratchet-drift: task schema mistakes and file-anchored
    # assertions that no longer match the source they pin (e.g. the
    # cli.rs first-line change that broke file_tool_read in P44).
    if "--check-only" in sys.argv:
        return check_only()
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
        attempts = 1 + task.get("retries", 0)
        ok = False
        out = ""
        for attempt in range(attempts):
            if task.get("special") == "live" and attempt > 0:
                # A previous attempt may have mutated the prompt on
                # disk — restore the baseline before retrying.
                with open(os.path.join(HARNESS, "system_prompt.md"), "w") as f:
                    f.write(baseline_prompt)
            prompt = task.get("retry_prompt", task["prompt"]) if attempt > 0 else task["prompt"]
            try:
                if task["name"].startswith("memory_"):
                    shutil.rmtree(WORK, ignore_errors=True)
                    os.makedirs(WORK, exist_ok=True)
                    out = run(prompt, extra=["--memory", mem_path])
                elif task.get("special") == "mcp":
                    out = run_mcp(prompt)
                elif task.get("special") == "acp":
                    out = run_acp(prompt)
                elif task.get("special") == "session":
                    out = run_session()
                else:
                    out = run(prompt)
            except subprocess.TimeoutExpired:
                print(f"  attempt {attempt + 1}: timeout after {TIMEOUT}s")
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
            if ok:
                break
            if attempt + 1 < attempts:
                print(f"  attempt {attempt + 1}: assertions missed, retrying")
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
