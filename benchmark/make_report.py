#!/usr/bin/env python3
"""Consolidated report for the overnight evolution benchmark.

Reads the latest sigil + dsh run results.json files and emits:
  - per-epoch verdict curves (pass / output-pass / fail) per task
  - capability-surface growth: sigil asset count over time vs dsh (frozen)
  - safety event summary
  - headline table

Output: ~/sigil_bench/REPORT.md (and stdout).
"""
import os, json, glob

HOME = os.path.expanduser("~")
BENCH = os.path.join(HOME, "sigil_bench2")
SIGIL_RUNS = os.path.join(BENCH, "runs")
DSH_RUNS   = os.path.join(BENCH, "dsh_runs")

def latest(path_glob):
    hits = glob.glob(path_glob)
    if not hits: return None
    return max(hits, key=os.path.getmtime)

def load_latest(base_dir):
    rd = latest(os.path.join(base_dir, "run_*", "results.json"))
    if not rd: return None, None
    return rd, json.load(open(rd, encoding="utf-8"))

def curve(results, ids):
    """{tid: [verdict per epoch]} preserving order."""
    out = {t: [] for t in ids}
    for r in results:
        t = r.get("task")
        if t in out: out[t].append(r.get("verdict", "?"))
    return out

def asset_timeline(results):
    """Cumulative 'added' asset count per epoch (sigil group only has deltas)."""
    by_ep = {}
    for r in results:
        ep = r.get("epoch")
        d = r.get("delta") or {}
        by_ep.setdefault(ep, {"added": 0, "changed": 0, "removed": 0})
        by_ep[ep]["added"]   += len(d.get("added", []))
        by_ep[ep]["changed"] += len(d.get("changed", []))
        by_ep[ep]["removed"] += len(d.get("removed", []))
    return by_ep

def main():
    sigil_file, sigil_res = load_latest(SIGIL_RUNS)
    dsh_file, dsh_res = load_latest(DSH_RUNS)
    lines = ["# 进化 benchmark 夜间报告", ""]
    if not sigil_res and not dsh_res:
        lines.append("（两组都没有结果文件——检查 run_night.log）")
        print("\n".join(lines)); return

    ids = ["T1_8", "T2", "T1_20"]

    if sigil_res:
        n = max(r.get("epoch", 1) for r in sigil_res)
        lines.append(f"## sigil（进化组）— {os.path.basename(os.path.dirname(sigil_file))}，{n} epochs")
        lines.append("")
        lines.append("| 任务 | " + " | ".join(f"ep{i}" for i in range(1, n+1)) + " |")
        lines.append("|---" * (len(ids)+1) + "|")
        c = curve(sigil_res, ids)
        for t in ids:
            row = c[t] + ["-"] * (n - len(c[t]))
            lines.append(f"| {t} | " + " | ".join(row) + " |")
        # pass rate per epoch
        lines.append("")
        per = {}
        for r in sigil_res:
            ep = r.get("epoch", 1)
            per.setdefault(ep, [0, 0])
            per[ep][1] += 1
            if r.get("verdict") in ("pass", "output-pass"): per[ep][0] += 1
        lines.append("每 epoch 通过率：" + "  ".join(f"ep{e}={v[0]}/{v[1]}" for e, v in sorted(per.items())))
        lines.append("")
        tl = asset_timeline(sigil_res)
        if tl:
            lines.append("能力面增长（每 epoch 新增/改进/删除资产数）：")
            for e in sorted(tl):
                a = tl[e]
                lines.append(f"  ep{e}: +{a['added']} / ~{a['changed']} / -{a['removed']}")
            lines.append("")
        safety = sorted({s for r in sigil_res for s in r.get("safety_flags", [])})
        lines.append(f"安全告警: {safety if safety else '无'}")
        lines.append("")

    if dsh_res:
        n = max(r.get("epoch", 1) for r in dsh_res)
        lines.append(f"## dsh（冻结对照组）— {os.path.basename(os.path.dirname(dsh_file))}，{n} epochs")
        lines.append("")
        lines.append("| 任务 | " + " | ".join(f"ep{i}" for i in range(1, n+1)) + " |")
        lines.append("|---" * (len(ids)+1) + "|")
        c = curve(dsh_res, ids)
        for t in ids:
            row = c[t] + ["-"] * (n - len(c[t]))
            lines.append(f"| {t} | " + " | ".join(row) + " |")
        per = {}
        for r in dsh_res:
            ep = r.get("epoch", 1)
            per.setdefault(ep, [0, 0])
            per[ep][1] += 1
            if r.get("verdict") in ("pass", "output-pass"): per[ep][0] += 1
        lines.append("")
        lines.append("每 epoch 通过率：" + "  ".join(f"ep{e}={v[0]}/{v[1]}" for e, v in sorted(per.items())))
        lines.append("（对照组无能力面增长——dsh 资产由人类编写，运行期冻结）")
        lines.append("⚠ 污染标注：dsh 的 T2 判定会扫到 sigil 组落盘的 harness-path.md，"
                     "其 T2 PASS 属继承而非独立取得——T2 对比无效，组间对比仅取 T1_8/T1_20。")
        lines.append("")

    out = "\n".join(lines)
    os.makedirs(BENCH, exist_ok=True)
    open(os.path.join(BENCH, "REPORT.md"), "w", encoding="utf-8").write(out)
    print(out)

if __name__ == "__main__":
    main()