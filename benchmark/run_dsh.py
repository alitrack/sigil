#!/usr/bin/env python3
"""dsh control-group driver — same task pool as sigil evolution benchmark.

dsh is the FROZEN-capability control: it answers each task but its harness
capability surface is static (human-authored plugins). sigil's surface grows.
Same tasks, same model (qwen3.8-27b), same endpoint (8014 sglang).

Usage:
  python run_dsh.py --tasks T1_8,T1_20,T2 --provider local-qwen-sglang

Scoring reuses tasks.py assertions (objective, no LLM self-grade).
"""
import os, sys, json, time, subprocess, argparse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

# The control group must live in its OWN data sandbox: the sigil driver
# pins task files under ~/sigil_bench, and the two groups must never share
# task data dirs (a pass could leak across groups).
DSH_WORKDIR = os.path.expanduser("~/sigil_bench_dsh")
os.makedirs(DSH_WORKDIR, exist_ok=True)
import tasks
tasks.WORKDIR = DSH_WORKDIR

# Where dsh persists its (human-authored, frozen) capability surface.
DSH_MEMORY = os.path.expanduser("~/.dsh/memory")

OUT = os.path.expanduser("~/sigil_bench/dsh_runs")
os.makedirs(OUT, exist_ok=True)

# Resolve the dsh executable to an absolute path — it lives in the npm
# global bin dir which may not be on the Python subprocess PATH. On
# Windows npm provides dsh.cmd (a batch shim); the bare `dsh` file is a
# POSIX shim that Python's CreateProcess cannot launch, so prefer .cmd.
import shutil
_cand = shutil.which("dsh")
if _cand and _cand.lower().endswith(".cmd"):
    DSH = _cand
else:
    DSH = os.path.join(os.path.expanduser("~"), "AppData", "Roaming", "npm", "dsh.cmd")
if not os.path.exists(DSH):
    raise SystemExit(f"dsh executable not found (tried {DSH}); install @deepseek-ai/dsh")

def run_task(tid, provider, timeout=420):
    t = tasks.TASKS[tid]
    if t.get("setup"): t["setup"]()
    prompt = t["prompt"]()
    open(os.path.join(OUT, f"prompt_{tid}.txt"), "w", encoding="utf-8").write(prompt)
    env = dict(os.environ)
    cmd = [DSH, "--profile", "headless"]
    if provider:
        patch = os.path.join(OUT, "patch_sglang.yml")
        with open(patch, "w", encoding="utf-8") as f:
            f.write(f'# pin default model to the sglang (8014) provider\n'
                    f'- target: agent-default-model\n'
                    f'  config:\n'
                    f'    provider: {provider}\n'
                    f'    model: qwen3.8-27b\n')
        cmd += ["--patch", patch]
    cmd.append(prompt)
    try:
        r = subprocess.run(cmd, env=env, capture_output=True, text=True,
                           encoding="utf-8", errors="ignore", timeout=timeout)
        out = (r.stdout or "") + "\n---STDERR---\n" + (r.stderr or "")
        code = r.returncode
    except subprocess.TimeoutExpired as e:
        so = e.stdout if isinstance(e.stdout, str) else (e.stdout or b"").decode("utf-8","ignore")
        se = e.stderr if isinstance(e.stderr, str) else (e.stderr or b"").decode("utf-8","ignore")
        out = so + "\n---STDERR---\n" + se + " [TIMEOUT]"
        code = -1
    open(os.path.join(OUT, f"out_{tid}.txt"), "w", encoding="utf-8").write(out)
    ok, why = t["assert"]()
    verdict = "pass" if ok else "fail"
    if t["family"] == "T1" and not ok:
        m = __import__("re").match(r"T1_(\d+)", tid)
        if m:
            ok2, why2 = tasks.t1_check_text(out, int(m.group(1)))
            if ok2:
                verdict, why = "output-pass", why2
    return {"task": tid, "exit": code, "pass": ok, "verdict": verdict, "why": why,
            "out_len": len(out)}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--epochs", type=int, default=1)
    ap.add_argument("--start-epoch", type=int, default=1,
                    help="first epoch number (resume support: dsh is stateless/frozen, "
                         "so a partial run can be continued and the verdict logs merged)")
    ap.add_argument("--tasks", default="T1_8,T2,T1_20")
    ap.add_argument("--provider", default="")
    ap.add_argument("--timeout", type=int, default=420)
    a = ap.parse_args()
    ids = [x for x in a.tasks.split(",") if x]
    results = []
    for ep in range(a.start_epoch, a.start_epoch + a.epochs):
        for tid in ids:
            res = run_task(tid, a.provider, a.timeout)
            res["epoch"] = ep
            results.append(res)
            print(f"[dsh][ep{ep}] {tid:8s} {res['verdict'].upper():12s} "
                  f"exit={res['exit']} {res['why'][:48]}", flush=True)
    rd = os.path.join(OUT, time.strftime("run_%Y%m%d_%H%M%S"))
    os.makedirs(rd, exist_ok=True)
    json.dump(results, open(os.path.join(rd, "results.json"), "w"), indent=1, ensure_ascii=False)
    print("\n=== dsh CONTROL SUMMARY (per task across epochs) ===")
    for tid in ids:
        eps = [r["verdict"] for r in results if r["task"] == tid]
        print(f"  {tid:8s} {eps}")
    print(f"run dir: {rd}")

if __name__ == "__main__":
    main()