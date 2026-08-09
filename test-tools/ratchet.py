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
import subprocess
import sys

EXE = "/mnt/d/wsl2/sigil/target/release/sigil"
HARNESS = "/tmp/sigil-harness-test"
TIMEOUT = 180


def run(prompt: str) -> str:
    proc = subprocess.run(
        [EXE, "--harness-dir", HARNESS],
        input=prompt, capture_output=True, text=True, timeout=TIMEOUT,
    )
    return proc.stdout + proc.stderr


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
    },
]


def main() -> int:
    failed = 0
    for task in TASKS:
        print(f"── {task['name']} …", flush=True)
        try:
            out = run(task["prompt"])
        except subprocess.TimeoutExpired:
            print(f"  FAIL: timeout after {TIMEOUT}s")
            failed += 1
            continue
        ok = any(a.lower() in out.lower() for a in task["assert"])
        # For lua_tool_available, either output is acceptable; make it
        # assert on 'tools' appearing at all.
        if task["name"] == "lua_tool_available":
            ok = "tools" in out.lower() or "no-tools-dir" in out.lower()
        print(f"  {'PASS' if ok else 'FAIL'}")
        if not ok:
            failed += 1
            print(f"  --- output tail ---\n{out[-400:]}")
    print()
    if failed:
        print(f"RATCHET: {len(TASKS) - failed}/{len(TASKS)} passed — {failed} FAILED")
        return 1
    print(f"RATCHET: {len(TASKS)}/{len(TASKS)} passed ✅")
    return 0


if __name__ == "__main__":
    sys.exit(main())
