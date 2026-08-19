#!/usr/bin/env python3
"""Resume the interrupted dsh control group (ep3-8), then merge the two
partial dsh logs into one canonical results.json and regenerate REPORT.md.

State note: dsh is the frozen-capability control (stateless per run, human
authored plugins only), so resuming from ep3 is methodologically clean —
no cross-epoch state was being accumulated in the control group.

Known contamination (documented in the report): dsh's T2 verdict scans the
sigil group's harness dir (harness-path.md persisted by sigil), so dsh T2
passes are inherited, not earned. T1_8 / T1_20 are unaffected (data sandboxes
are isolated). The merged report flags dsh T2 accordingly.
"""
import os, re, sys, time, json, subprocess, glob

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
PY = sys.executable
HOME = os.path.expanduser("~")
BENCH = os.path.join(HOME, "sigil_bench")

def check_endpoint():
    import urllib.request
    try:
        r = urllib.request.urlopen("http://10.10.10.115:8014/v1/models", timeout=15)
        print(f"endpoint 8014 status={r.status}", flush=True)
        return r.status == 200
    except Exception as e:
        print(f"endpoint 8014 UNREACHABLE: {e}", flush=True)
        return False

def parse_dsh_log(path):
    """Parse '[dsh][epN] TID  VERDICT  exit=X why...' lines -> records."""
    recs = []
    if not os.path.exists(path): return recs
    pat = re.compile(r"^\[dsh\]\[ep(\d+)\]\s+(\S+)\s+(\S+)\s+exit=(-?\d+)\s*(.*)$")
    for line in open(path, encoding="utf-8", errors="ignore"):
        m = pat.match(line.strip())
        if m:
            recs.append({"epoch": int(m.group(1)), "task": m.group(2),
                         "verdict": m.group(3).lower(), "exit": int(m.group(4)),
                         "why": m.group(5).strip()})
    return recs

def main():
    if not check_endpoint():
        raise SystemExit("endpoint down — aborting resume; rerun after 8014 is back")

    # 1) resume dsh ep3-8
    resume_log = os.path.join(BENCH, "dsh_resume.log")
    print(f"=== {time.strftime('%H:%M:%S')} START dsh resume (ep3-8)", flush=True)
    r = subprocess.run(
        [PY, "benchmark/run_dsh.py", "--epochs", "6", "--start-epoch", "3",
         "--tasks", "T1_8,T2,T1_20", "--provider", "local-qwen-sglang",
         "--timeout", "900"],
        stdout=open(resume_log, "ab"), stderr=subprocess.STDOUT, cwd=REPO)
    print(f"=== {time.strftime('%H:%M:%S')} dsh resume done exit={r.returncode}", flush=True)

    # 2) merge the two dsh logs (ep1-2 from dsh_night.log, ep3-8 from resume)
    merged = []
    seen = set()
    for path in (os.path.join(BENCH, "dsh_night.log"), resume_log):
        for rec in parse_dsh_log(path):
            key = (rec["epoch"], rec["task"])
            if key in seen: continue
            seen.add(key)
            merged.append(rec)
    merged.sort(key=lambda x: (x["epoch"], x["task"]))
    rd = os.path.join(BENCH, "dsh_runs", "run_merged")
    os.makedirs(rd, exist_ok=True)
    json.dump(merged, open(os.path.join(rd, "results.json"), "w"),
              indent=1, ensure_ascii=False)
    print(f"merged {len(merged)} dsh records -> {rd}", flush=True)

    # 3) regenerate consolidated report
    subprocess.run([PY, "benchmark/make_report.py"], cwd=REPO)
    print("RESUME_DONE", flush=True)

if __name__ == "__main__":
    main()