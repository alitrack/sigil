#!/usr/bin/env python3
"""Overnight evolution benchmark orchestrator.

Group 1 (sigil): evolving capability surface — tools/harness persist and
grow across epochs (the experimental group).
Group 2 (dsh):   frozen capability surface — same tasks/model/endpoint,
human-authored plugins only (the control group).

Sequential (single local GPU backend); per-task lines are flushed so
progress can be tailed from the log mid-run.
"""
import os, sys, subprocess, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
PY = sys.executable

SIGIL_EPOCHS = int(os.environ.get("NIGHT_SIGIL_EPOCHS", "8"))
DSH_EPOCHS   = int(os.environ.get("NIGHT_DSH_EPOCHS", "8"))
TASKS = "T1_8,T2,T1_20"
BASE = "http://10.10.10.115:8014/v1"
MODEL = "qwen3.8-27b"
TIMEOUT = "900"
MAXSTEPS = "150"

def run(cmd, logfile):
    print(f"=== {time.strftime('%H:%M:%S')} START: {cmd}", flush=True)
    t0 = time.time()
    with open(logfile, "ab") as lf:
        r = subprocess.run(cmd, stdout=lf, stderr=subprocess.STDOUT, cwd=REPO)
    print(f"=== {time.strftime('%H:%M:%S')} DONE  ({int(time.time()-t0)/60:.0f} min) exit={r.returncode}", flush=True)
    return r.returncode

def main():
    # 1) sigil experimental group
    c1 = run([PY, "benchmark/run_bench.py",
              "--epochs", str(SIGIL_EPOCHS), "--tasks", TASKS,
              "--model", MODEL, "--base-url", BASE,
              "--timeout", TIMEOUT, "--max-steps", MAXSTEPS],
             os.path.expanduser("~/sigil_bench/sigil_night.log"))
    # 2) dsh control group (own data sandbox via SIGIL_BENCH_WORKDIR override)
    c2 = run([PY, "benchmark/run_dsh.py",
              "--epochs", str(DSH_EPOCHS), "--tasks", TASKS,
              "--provider", "local-qwen-sglang", "--timeout", TIMEOUT],
             os.path.expanduser("~/sigil_bench/dsh_night.log"))
    # 3) consolidated report
    run([PY, "benchmark/make_report.py"],
        os.path.expanduser("~/sigil_bench/report.log"))
    print("NIGHT_RUN_DONE", flush=True)
    sys.exit(0 if (c1 == 0 and c2 == 0) else 1)

if __name__ == "__main__":
    main()