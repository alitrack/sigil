#!/usr/bin/env python3
"""sigil evolution benchmark — round 3 orchestrator (two-phase, multi-seed).

Phase A (evolution): per seed, cold start, 6 epochs x T1x,T4,T2 — the model
  may freely create tools / edit harness / write memory (round-2 protocol).

Phase B (held-out exam): SAME seed asset dirs, SIGIL_FREEZE=1 so evolution
  writes (create_lua_tool / improve_harness / remember) are rejected by the
  binary itself; tasks H1,H2 only. Scores measure TRANSFER of the evolved
  capability surface, not further evolution.

Control group (dsh): runs H1,H2 (and optionally the training tasks once) with
  its own per-seed sandbox, same model/endpoint — the round-2 frozen protocol.

Usage:
  python run_night3.py --phase A --seeds s1,s2,s3 \
      --model qwen3.8-27b --base-url http://10.10.10.115:8014/v1 \
      --base ~/sigil_bench3
  python run_night3.py --phase B --seeds s1,s2,s3 ...   # after A finished
  python run_night3.py --phase dsh --seeds s1,s2,s3 ... # control H1,H2
"""
import os, sys, subprocess, argparse, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)

def run(cmd, env_extra=None, log_path=None):
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    with open(log_path, "a", encoding="utf-8") as lf:
        lf.write(f"\n$ {' '.join(cmd)}\n")
        lf.flush()
        r = subprocess.run(cmd, env=env, stdout=lf, stderr=subprocess.STDOUT)
    return r.returncode

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--phase", required=True, choices=["A", "B", "dsh"])
    ap.add_argument("--seeds", default="s1,s2,s3")
    ap.add_argument("--epochs", type=int, default=6)
    ap.add_argument("--model", default="qwen3.8-27b")
    ap.add_argument("--base-url", default="http://10.10.10.115:8014/v1")
    ap.add_argument("--api-key", default="local-no-auth")
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--base", default=os.path.expanduser("~/sigil_bench3"))
    a = ap.parse_args()
    seeds = [s.strip() for s in a.seeds.split(",") if s.strip()]
    os.makedirs(a.base, exist_ok=True)
    master_log = os.path.join(a.base, f"night3_{a.phase}.log")

    for s in seeds:
        t0 = time.time()
        log_path = master_log if master_log else os.path.join(a.base, "night3.log")
        if a.phase == "A":
            # Evolution: same driver/protocol as round 2, new task pool.
            rc = run([sys.executable, os.path.join(HERE, "run_bench.py"),
                      "--seed", s, "--epochs", str(a.epochs),
                      "--tasks", "T1x,T4,T2",
                      "--model", a.model, "--base-url", a.base_url,
                      "--api-key", a.api_key,
                      "--timeout", str(a.timeout),
                      "--out", os.path.join(a.base, "runs")],
                     log_path=master_log)
        elif a.phase == "B":
            # Held-out exam on the SAME asset dirs (--out identical to A) with
            # SIGIL_FREEZE=1. One epoch: each held-out task once per seed.
            rc = run([sys.executable, os.path.join(HERE, "run_bench.py"),
                      "--seed", s, "--epochs", "1",
                      "--tasks", "H1,H2",
                      "--model", a.model, "--base-url", a.base_url,
                      "--api-key", a.api_key,
                      "--timeout", str(a.timeout),
                      "--out", os.path.join(a.base, "runs")],
                     env_extra={"SIGIL_FREEZE": "1"},
                     log_path=master_log)
        else:  # dsh control on held-out (frozen by design: no evolution tools)
            rc = run([sys.executable, os.path.join(HERE, "run_dsh.py"),
                      "--seed", s, "--epochs", "1",
                      "--tasks", "H1,H2",
                      "--provider", "local-qwen-sglang",
                      "--timeout", str(a.timeout),
                      "--out", os.path.join(a.base, "dsh_runs")],
                     log_path=master_log)
        dt = time.time() - t0
        print(f"[phase {a.phase}] seed {s}: rc={rc} ({dt:.0f}s)", flush=True)

    # consolidated report
    rc = run([sys.executable, os.path.join(HERE, "make_report3.py"),
              "--base", a.base], log_path=master_log)
    print(f"report rc={rc}; log: {master_log}", flush=True)

if __name__ == "__main__":
    main()
