#!/usr/bin/env python3
"""dsh control-group driver (round 2: sandbox-aligned + per-seed).

dsh is the FROZEN-capability control: it answers each task but its capability
surface (human-authored plugins/tools) is static. Same task pool, same model
(qwen3.8-27b), same endpoint (8014 sglang) as the sigil evolution group.

Round-2 fixes over round 1:
  1. The dsh process CWD is set to this seed's data sandbox, so the
     sandbox-policy `workspaceRoot = process.cwd()` lines up with where the
     task files (and the SUMMARY.txt answer) actually live. In round 1 the
     process inherited the repo CWD, so dsh "could only write D:\\wsl2\\sigil"
     and its correct T1 answers never landed — a mechanical false-failure, not
     a capability gap.
  2. Control group runs the group-comparable tasks only (T1_8, T1_20). T2
     (memory persistence) is structurally non-comparable for dsh: its
     local-memory store is a global singleton that cannot be isolated per
     seed, so its T2 verdict was cross-contaminated in round 1 and is dropped
     from the inter-group comparison.
  3. Per-seed data sandbox under ~/sigil_bench2/dsh/seed_<s>/data.

Usage:
  python run_dsh.py --seed s1 --epochs 6 --tasks T1_8,T1_20 \
      --provider local-qwen-sglang --timeout 900 --out ~/sigil_bench2/dsh_runs
"""
import os, sys, json, time, subprocess, argparse, shutil, re

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

def setup_seed(seed, out_base):
    """Point tasks.py at this seed's data sandbox and prepare its out dir."""
    base = os.path.join(out_base, f"seed_{seed}")
    data = os.path.join(base, "data")
    runs = os.path.join(base, "runs")
    for d in (base, data, runs):
        os.makedirs(d, exist_ok=True)
    import tasks
    tasks.WORKDIR = data
    # T2 is dropped from the control group; if it is ever run, credit only
    # dsh's own (global) memory, never sigil's asset dirs.
    tasks.T2_PERSIST_ROOTS = []
    tasks.T2_MEMORY_FILES = [
        os.path.expanduser("~/.dsh/memory/memories.jsonl"),
        os.path.expanduser("~/.dsh/memory/lessons.jsonl"),
    ]
    return {"SEED": seed, "DATA": data, "RUNS": runs}

# Resolve the dsh executable to an absolute path — npm global bin dir may not
# be on the Python subprocess PATH. On Windows npm provides dsh.cmd (batch
# shim); the bare `dsh` is a POSIX shim CreateProcess cannot launch.
_cand = shutil.which("dsh")
if _cand and sys.platform == "win32" and not _cand.lower().endswith(".cmd"):
    _cand = None  # POSIX shim unusable via CreateProcess on Windows
if _cand:
    DSH = _cand
elif sys.platform == "win32":
    DSH = os.path.join(os.path.expanduser("~"), "AppData", "Roaming", "npm", "dsh.cmd")
else:
    DSH = "dsh"  # POSIX: rely on PATH
if sys.platform != "win32" and shutil.which("dsh") is None:
    raise SystemExit("dsh executable not found on PATH; install @deepseek-ai/dsh")

def run_task(st, tid, provider, timeout=900):
    import tasks
    t = tasks.TASKS[tid]
    if t.get("setup"): t["setup"]()
    prompt = t["prompt"]()
    open(os.path.join(st["RUNS"], f"prompt_{tid}.txt"), "w", encoding="utf-8").write(prompt)
    env = dict(os.environ)
    env["MSYS_NO_PATHCONV"] = "1"
    env["MSYS2_ARG_CONV_EXCL"] = "*"
    # pi-ai's credential lookup for the local route (gateway needs no real key)
    env["LOCAL_QWEN_API_KEY"] = env.get("LOCAL_QWEN_API_KEY", "local-no-auth")
    cmd = [DSH, "--profile", "headless"]
    if provider:
        patch = os.path.join(st["RUNS"], "patch_sglang.yml")
        # VERIFIED mechanism (2026-08-20, dsh 0.1.0-rc.7 headless): the
        # llm-pi-ai adapter mounts DORMANT in dsh-base — zero routes until
        # its `providers` config (the `llm-pi-ai:` settings section) supplies
        # provider profiles. The headless profile does NOT see the global
        # ~/.dsh/settings.yaml section at runtime, so pinning only
        # agent-default-model dies with NO_ADAPTER. The --patch overlay must
        # (1) pin the default model and (2) declare the route in the adapter
        # config. `id:`-targeted rows are the schema the include loader
        # applies; `target:` rows are silently ignored.
        models = "        models:\n          - id: qwen3.8-27b\n            name: Qwen3.8 27B (sglang)\n            contextWindow: 262144\n            maxTokens: 32768\n"
        with open(patch, "w", encoding="utf-8") as f:
            f.write("# pin default model to the local sglang (8014) provider\n"
                    f"- id: agent-default-model\n"
                    f"  config:\n"
                    f"    provider: {provider}\n"
                    f"    model: qwen3.8-27b\n"
                    "# declare the route in the (dormant) pi-ai adapter so it registers\n"
                    f"- id: llm-pi-ai\n"
                    f"  config:\n"
                    f"    providers:\n"
                    f"      {provider}:\n"
                    f"        displayName: Local Qwen SGLang (10.10.10.115:8014)\n"
                    f"        apiKeyEnv: LOCAL_QWEN_API_KEY\n"
                    f"        api: openai-completions\n"
                    f"        baseURL: http://10.10.10.115:8014/v1\n"
                    f"{models}"
                    "# isolate control group from global memory (see driver docstring)\n"
                    f"- id: local-memory\n"
                    f"  disabled: true\n")
        cmd += ["--patch", patch]
    cmd.append(prompt)
    # KEY: cwd = seed data sandbox => sandbox-policy workspaceRoot aligns with
    # where the task files and the SUMMARY.txt answer live.
    out, code = "", 0
    for attempt in range(4):  # endpoint queue saturation backoff (par. sigil)
        try:
            r = subprocess.run(cmd, env=env, cwd=st["DATA"], capture_output=True,
                               text=True, encoding="utf-8", errors="ignore", timeout=timeout)
            out = (r.stdout or "") + "\n---STDERR---\n" + (r.stderr or "")
            code = r.returncode
        except subprocess.TimeoutExpired as e:
            so = e.stdout if isinstance(e.stdout, str) else (e.stdout or b"").decode("utf-8","ignore")
            se = e.stderr if isinstance(e.stderr, str) else (e.stderr or b"").decode("utf-8","ignore")
            out = so + "\n---STDERR---\n" + se + " [TIMEOUT]"
            code = -1
        if "request_queue_timeout" in out or "503" in out.split("---STDERR---")[-1][:400]:
            if attempt < 3:
                wait = 60 * (attempt + 1)
                print(f"  [dsh retry {attempt+1}] queue saturated, backoff {wait}s", flush=True)
                time.sleep(wait)
                continue
            out += " [GAVE_UP_ON_QUEUE]"
        break
    open(os.path.join(st["RUNS"], f"out_{tid}.txt"), "w", encoding="utf-8").write(out)
    ok, why = t["assert"]()
    verdict = "pass" if ok else "fail"
    if t["family"] == "T1" and not ok:
        import re
        m = re.match(r"T1_(\d+)", tid)
        if m:
            ok2, why2 = tasks.t1_check_text(out, int(m.group(1)))
            if ok2:
                verdict, why = "output-pass", why2
    return {"task": tid, "exit": code, "pass": ok, "verdict": verdict, "why": why,
            "out_len": len(out), "seed": st["SEED"]}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", default="s0")
    ap.add_argument("--epochs", type=int, default=1)
    ap.add_argument("--start-epoch", type=int, default=1)
    ap.add_argument("--tasks", default="T1_8,T1_20")
    ap.add_argument("--provider", default="local-qwen-sglang")
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--out", default=os.path.expanduser("~/sigil_bench2/dsh_runs"))
    a = ap.parse_args()
    # git-bash/MSYS mangles POSIX-style args: '~' arrives as '/c/Users/...',
    # which Python treats as a RELATIVE path on Windows and files under the
    # CWD (e.g. D:\c\Users\...). Re-pin to the real drive so data never
    # mis-files regardless of which shell launched us.
    m = re.match(r"^/([a-zA-Z])(/.*)$", a.out)
    if m and os.path.isdir(m.group(1).upper() + ":"):
        a.out = m.group(1).upper() + ":\\" + m.group(2).lstrip("/")
    ids = [x for x in a.tasks.split(",") if x]
    st = setup_seed(a.seed, a.out)
    results = []
    for ep in range(a.start_epoch, a.start_epoch + a.epochs):
        for tid in ids:
            res = run_task(st, tid, a.provider, a.timeout)
            res["epoch"] = ep
            results.append(res)
            print(f"[dsh][{a.seed}][ep{ep}] {tid:8s} {res['verdict'].upper():12s} "
                  f"exit={res['exit']} {res['why'][:48]}", flush=True)
    rd = os.path.join(st["RUNS"])
    json.dump(results, open(os.path.join(rd, "results.json"), "w"), indent=1, ensure_ascii=False)
    print(f"\n=== dsh CONTROL SUMMARY seed={a.seed} (per task across epochs) ===")
    for tid in ids:
        eps = [r["verdict"] for r in results if r["task"] == tid]
        print(f"  {tid:8s} {eps}")
    print(f"run dir: {rd}")

if __name__ == "__main__":
    main()