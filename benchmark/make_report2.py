#!/usr/bin/env python3
"""Round-2 consolidated report (multi-seed).

Reads per-seed results.json from --base (default ~/sigil_bench2):
  <base>/runs/seed_<s>/runs/results.json     (sigil evolution group)
  <base>/dsh_runs/seed_<s>/runs/results.json (dsh frozen control)

Emits:
  - per-seed per-task verdict curve + per-epoch pass rate + asset growth
  - cross-seed aggregate: for each seed the T1 pass counts (replicability)
  - inter-group T1 comparison (T2 excluded: non-comparable for dsh)
  - safety events per group

Output: <base>/REPORT2.md (and stdout).

Usage:
  python make_report2.py --base ~/sigil_bench2
"""
import os, sys, json, glob, argparse

def load(base, group, seed):
    p = os.path.join(base, "runs" if group == "sigil" else "dsh_runs",
                     f"seed_{seed}", "runs", "results.json")
    if not os.path.exists(p):
        return None
    return json.load(open(p, encoding="utf-8"))

def passrate(results, ep):
    rows = [r for r in results if r.get("epoch") == ep]
    if not rows:
        return (0, 0)
    hit = sum(1 for r in rows if r.get("verdict") in ("pass", "output-pass"))
    return (hit, len(rows))

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=os.path.expanduser("~/sigil_bench2"))
    ap.add_argument("--seeds", default="s1,s2,s3")
    a = ap.parse_args()
    seeds = [s.strip() for s in a.seeds.split(",") if s.strip()]
    L = ["# 进化 benchmark 第 2 轮报告(多 seed)", ""]

    sig = {s: load(a.base, "sigil", s) for s in seeds}
    dsh = {s: load(a.base, "dsh", s) for s in seeds}
    nseeds = len(seeds)

    # ---- sigil evolution group: per-seed curves + cross-seed aggregate ----
    L.append(f"## sigil 进化组(冷启动 × {nseeds} seed)")
    L.append("")
    for s in seeds:
        r = sig[s]
        if not r:
            L.append(f"- seed {s}: 无结果(未跑或中断)")
            continue
        ids = ["T1_8", "T2", "T1_20"]
        n = max((x.get("epoch", 1) for x in r), default=0)
        L.append(f"### seed {s} — {n} epochs")
        L.append("")
        L.append("| 任务 | " + " | ".join(f"ep{i}" for i in range(1, n+1)) + " |")
        L.append("|---" * (len(ids)+1) + "|")
        for t in ids:
            row = [next((x["verdict"] for x in r if x["task"] == t and x.get("epoch") == i), "-")
                   for i in range(1, n+1)]
            L.append(f"| {t} | " + " | ".join(row) + " |")
        per = [passrate(r, i) for i in range(1, n+1)]
        L.append("")
        L.append("每 epoch 通过率：" + "  ".join(f"ep{i+1}={h}/{c}" for i, (h, c) in enumerate(per)))
        tl = {}
        for x in r:
            d = x.get("delta") or {}
            tl.setdefault(x.get("epoch"), [0, 0, 0])
            tl[x.get("epoch")][0] += len(d.get("added", []))
            tl[x.get("epoch")][1] += len(d.get("changed", []))
            tl[x.get("epoch")][2] += len(d.get("removed", []))
        cum = 0
        for e in sorted(tl):
            cum += tl[e][0]
        L.append(f"累计新增资产(到 ep{n}): {cum}")
        L.append("")
        L.append("每任务通过率：" + "  ".join(
            f"{t}={sum(1 for x in r if x['task']==t and x.get('verdict') in ('pass','output-pass'))}/{n}"
            for t in ids))
        safety = sorted({x for x in (f for row in (sig[s] or []) for x in row.get("safety_flags", []))})
        L.append(f"安全告警: {safety if safety else '无'}")
        L.append("")

    # cross-seed replicability: T1 series pass-counts per seed (sigil)
    L.append("### 跨 seed 可复现性(T1 系列通过率,3 条独立冷启动曲线)")
    L.append("")
    L.append("| seed | T1_8 | T2 | T1_20 | 总通过率 | 累计资产 |")
    L.append("|---|---|---|---|---|---|")
    for s in seeds:
        r = sig[s]
        if not r:
            L.append(f"| {s} | — | — | — | — | — |")
            continue
        n = max((x.get("epoch", 1) for x in r), default=0)
        def pr(t):
            return f"{sum(1 for x in r if x['task']==t and x.get('verdict') in ('pass','output-pass'))}/{n}"
        tot = sum(sum(1 for x in r if x.get('verdict') in ('pass','output-pass'))
                  for _ in [0]) / (3 * n or 1)
        cum = sum(len((x.get("delta") or {}).get("added", [])) for x in r)
        L.append(f"| {s} | {pr('T1_8')} | {pr('T2')} | {pr('T1_20')} | {tot:.0%} | {cum} |")
    L.append("")

    # ---- dsh frozen control ----
    L.append("## dsh 冻结对照组(T1 系列,sandbox 对齐,local-memory 关闭)")
    L.append("")
    for s in seeds:
        r = dsh[s]
        if not r:
            L.append(f"- seed {s}: 无结果(未跑或中断)")
            continue
        ids = ["T1_8", "T1_20"]
        n = max((x.get("epoch", 1) for x in r), default=0)
        L.append(f"### seed {s} — {n} epochs")
        L.append("")
        L.append("| 任务 | " + " | ".join(f"ep{i}" for i in range(1, n+1)) + " |")
        L.append("|---" * (len(ids)+1) + "|")
        for t in ids:
            row = [next((x["verdict"] for x in r if x["task"] == t and x.get("epoch") == i), "-")
                   for i in range(1, n+1)]
            L.append(f"| {t} | " + " | ".join(row) + " |")
        L.append("")
        L.append("每任务通过率：" + "  ".join(
            f"{t}={sum(1 for x in r if x['task']==t and x.get('verdict') in ('pass','output-pass'))}/{n}"
            for t in ids))
        L.append("")
    L.append("（对照组能力面运行期冻结;local-memory 已关闭以隔离全局记忆）")
    L.append("")

    # ---- inter-group T1 comparison (cross-seed) ----
    L.append("## 组间对比(仅 T1 系列,跨 seed 平均)")
    L.append("")
    L.append("| 任务 | sigil 平均通过/seed | dsh 平均通过/seed |")
    L.append("|---|---|---|")
    for t in ("T1_8", "T1_20"):
        def avg(group, task):
            vals = []
            for s in seeds:
                r = group[s]
                if not r:
                    continue
                n = max((x.get("epoch", 1) for x in r), default=0)
                if n:
                    vals.append(sum(1 for x in r if x["task"] == task and x.get("verdict") in ("pass","output-pass")) / n)
            return (sum(vals) / len(vals) if vals else float("nan")), len(vals)
        (sv, sc) = avg(sig, t)
        (dv, dc) = avg(dsh, t)
        L.append(f"| {t} | {sv:.0%} (n={sc}) | {dv:.0%} (n={dc}) |")
    L.append("")
    L.append("⚠ T2 组间对比无效:dsh 的 local-memory 为全局单例,无法按 seed 隔离且属用户 live 会话;round2 已将其从对照组移除。")
    L.append("")

    out = "\n".join(L)
    os.makedirs(a.base, exist_ok=True)
    open(os.path.join(a.base, "REPORT2.md"), "w", encoding="utf-8").write(out)
    print(out)

if __name__ == "__main__":
    main()