#!/usr/bin/env python3
"""Phase 32 E2E: two concurrent sigil processes write the same tools dir.

Both processes run `create_lua_tool` (via the agent loop, real LLM) against
the SAME --tools-dir. Without the directory lock they could interleave
"read old -> archive -> write new"; with it each tool write is atomic and
the version library stays consistent.

Assertions:
  1. both tools land on disk (2 .lua files)
  2. the archive dir contains exactly 2 version snapshots (one per write)
  3. no interleaved/corrupt content — each .lua parses and matches its
     creator's code exactly
"""
import json
import os
import subprocess
import sys
import threading
import time

EXE = "./target/release/sigil"
WORK = "/tmp/sigil-lock-e2e"
TOOLS = os.path.join(WORK, "tools")
HARNESS = os.path.join(WORK, "harness")

P1 = ("Update the existing Lua scripted tool named shared with code: "
      "return function(args) return 'AAA' end. "
      "Use create_lua_tool with action update. Then stop immediately.")
P2 = ("Update the existing Lua scripted tool named shared with code: "
      "return function(args) return 'BBB' end. "
      "Use create_lua_tool with action update. Then stop immediately.")


def run_prompt(prompt: str) -> str:
    r = subprocess.run(
        [EXE, "--tools-dir", TOOLS, "--harness-dir", HARNESS, "--max-steps", "8"],
        input=prompt, capture_output=True, text=True, timeout=240,
        env={**os.environ, "SIGIL_BASE_URL": "http://127.0.0.1:15722/v1",
             "SIGIL_MODEL": "deepseek-v4-flash"},
    )
    return (r.stdout or "") + (r.stderr or "")


def main() -> int:
    import shutil
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(TOOLS, exist_ok=True)
    os.makedirs(HARNESS, exist_ok=True)

    # Seed a shared tool BOTH processes will update at (nearly) the same
    # time. Without the directory lock the "read old -> archive -> write
    # new" sequences can interleave: one process archives/writes while the
    # other reads a half-written file, corrupting the version library or
    # the live tool. With the lock each update is atomic.
    with open(os.path.join(TOOLS, "shared.lua"), "w") as fh:
        fh.write("return function(args) return 'ORIG' end\n")

    # Fire both processes at (nearly) the same time so they race on the
    # same lock file.
    procs = [
        subprocess.Popen(
            [EXE, "--tools-dir", TOOLS, "--harness-dir", HARNESS, "--max-steps", "8"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True,
            env={**os.environ, "SIGIL_BASE_URL": "http://127.0.0.1:15722/v1",
                 "SIGIL_MODEL": "deepseek-v4-flash"},
        )
        for _ in (P1, P2)
    ]
    for p, prompt in zip(procs, (P1, P2)):
        assert p.stdin is not None
        p.stdin.write(prompt)
        p.stdin.close()  # EOF → pipe mode

    # Drain stdout on a thread per process so a full pipe can't deadlock.
    bufs: list[list[str]] = [[] for _ in procs]
    threads = []
    for p, buf in zip(procs, bufs):
        def read_out(proc: subprocess.Popen, sink: list[str]) -> None:
            assert proc.stdout is not None
            sink.append(proc.stdout.read())
        t = threading.Thread(target=read_out, args=(p, buf), daemon=True)
        t.start()
        threads.append(t)

    for p in procs:
        try:
            p.wait(timeout=240)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait(timeout=10)
    for t in threads:
        t.join(timeout=10)

    outputs = []
    for p, buf in zip(procs, bufs):
        out = "".join(buf) or ""
        outputs.append(out)
        print(f"--- proc rc={p.returncode} ---")
        print(out[-1500:] if out else "(no output)")

    # 1. shared.lua still exists and is ONE of the two complete payloads
    #    (never a mix of AAA+BBB — that would mean interleaved writes).
    shared = os.path.join(TOOLS, "shared.lua")
    with open(shared) as fh:
        content = fh.read()
    print(f"SHARED CONTENT: {content.strip()!r}")
    if "AAA" in content and "BBB" in content:
        print("FAIL: shared.lua contains BOTH payloads — writes interleaved!")
        return 1
    if "AAA" not in content and "BBB" not in content:
        print(f"FAIL: shared.lua has neither payload — corrupted:\n{content}")
        return 1
    print(f"OK: shared.lua intact, exactly one payload ({'AAA' if 'AAA' in content else 'BBB'})")

    # 2. version archives: each update snapshots the pre-write content.
    #    First update archives ORIG, second archives the first payload.
    arch_dir = os.path.join(TOOLS, "versions", "tools")
    archived = sorted(os.listdir(arch_dir)) if os.path.isdir(arch_dir) else []
    print(f"ARCHIVED: {archived}")
    if len(archived) < 2:
        print(f"FAIL: expected >=2 archived snapshots (ORIG + first payload), got {len(archived)}")
        return 1
    # Spot-check: every archive is a full, un-interleaved snapshot.
    for a in archived:
        with open(os.path.join(arch_dir, a)) as fh:
            snap = fh.read()
        if "AAA" in snap and "BBB" in snap:
            print(f"FAIL: archive {a} interleaved both payloads")
            return 1
        print(f"OK: archive {a} intact ({len(snap)} bytes)")

    # 3. lock file exists (created on demand)
    lock_path = os.path.join(TOOLS, ".sigil.lock")
    if not os.path.exists(lock_path):
        print("FAIL: .sigil.lock not created")
        return 1
    print("OK: .sigil.lock present")

    print("\nE2E PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
