#!/usr/bin/env python3
"""Round-2 evolution benchmark orchestrator (multi-seed).

Per seed:
  1) sigil evolution group  — T1_8,T2,T1_20 over N epochs (capability surface
     persists & grows across epochs; fully seed-isolated asset dirs).
  2) dsh frozen control      — T1_8,T1_20 over N epochs (group-comparable T1
     tasks only; sandbox cwd aligned; local-memory disabled to keep the frozen
     control off the user's global dsh memory).

Seeds run strictly sequentially (single local GPU backend); per-seed lines are
flushed so progress can be tailed mid-run. A seed that is interrupted can be
resumed by re-running with the remaining seeds (each seed's dirs are created
idempotently and results.json is appended per seed).

Config via env:
  NIGHT2_SEEDS   comma list, default "s1,s2,s3"
  NIGHT2_EPOCHS  int, default 6
  NIGHT2_TASKS_SIGIL  default "T1_8,T2,T1_20"
  NIGHT2_TASKS_DSH    default "T1_8,T1_20"
"""
import os, sys, subprocess, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
PY = sys.executable

SEEDS = os.environ.get("NIGHT2_SEEDS", "s1,s2,s3").split(",")
EPOCHS = int(os.environ.get("NIGHT2_EPOCHS", "6"))
SIGIL_TASKS = os.environ.get("NIGHT2_TASKS_SIGIL", "T1_8,T2,T1_20")
DSH_TASKS   = os.environ.get("NIGHT2_TASKS_DSH", "T1_8,T1_20")
BASE = "http://10.10.10.115:8014/v1"
MODEL = "qwen3.8-27b"
TIMEOUT = os.environ.get("NIGHT2_TIMEOUT", "900")
MAXSTEPS = os.environ.get("NIGHT2_MAXSTEPS", "150")
OUT = os.path.expanduser("~/sigil_bench2")

def run(cmd, logfile):
    print(f"=== {time.strftime('%m-%d %H:%M:%S')} START: {' '.join(cmd)}", flush=True)
    t0 = time.time()
    with open(logfile, "ab") as lf:
        r = subprocess.run(cmd, stdout=lf, stderr=subprocess.STDOUT, cwd=REPO)
    print(f"=== {time.strftime('%m-%d %H:%M:%S')} DONE  ({int(time.time()-t0)/60:.0f} min) exit={r.returncode}", flush=True)
    return r.returncode

def main():
    os.makedirs(OUT, exist_ok=True)
    rc = 0
    for seed in SEEDS:
        tag = seed.strip()
        # 1) sigil experimental (evolving) group
        c1 = run([PY, "benchmark/run_bench.py",
                  "--seed", tag, "--epochs", str(EPOCHS), "--tasks", SIGIL_TASKS,
                  "--model", MODEL, "--base-url", BASE,
                  "--timeout", TIMEOUT, "--max-steps", MAXSTEPS,
                  "--out", os.path.join(OUT, "runs")],
                 os.path.join(OUT, f"sigil_{tag}.log"))
        # 2) dsh frozen control group (T1 tasks only, sandbox-aligned)
        c2 = run([PY, "benchmark/run_dsh.py",
                  "--seed", tag, "--epochs", str(EPOCHS), "--tasks", DSH_TASKS,
                  "--provider", "local-qwen-sglang", "--timeout", TIMEOUT,
                  "--out", os.path.join(OUT, "dsh_runs")],
                 os.path.join(OUT, f"dsh_{tag}.log"))
        rc |= (c1 if c1 else 0) | (c2 if c2 else 0)
    run([PY, "benchmark/make_report2.py", "--base", OUT],
        os.path.join(OUT, "report.log"))
    print("ROUND2_DONE", flush=True)
    sys.exit(rc)

if __name__ == "__main__":
    main()