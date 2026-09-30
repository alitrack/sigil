#!/usr/bin/env python3
"""Round-3 consolidated report: cost-per-pass + held-out transfer.

Reads <base>/runs/seed_<s>/runs/results.json  (sigil; phase A+B rows are
appended by run_bench.py into the same file) and <base>/dsh_runs/.../results.json.

Emits <base>/REPORT3.md:
  - Phase A per-seed: pass rate, asset growth, cost trend across epochs
    (cost = tokens & steps & wall_s per task; evolution should LOWER it)
  - Phase B (held-out H1/H2): sigil-frozen vs dsh, pass + cost
  - headline: cost-per-pass ratio (sigil vs dsh) on held-out tasks
"""
import os, sys, json, argparse

def load(base, group, seed):
    p = os.path.join(base, "runs" if group == "sigil" else "dsh_runs",
                     f"seed_{seed}", "runs", "results.json")
    if not os.path.exists(p):
        return None
    try:
        return json.load(open(p, encoding="utf-8"))
    except Exception:
        return None

def cost_of(row):
    tok = (row.get("prompt_tokens") or 0) + (row.get("completion_tokens") or 0)
    return tok, row.get("steps"), row.get("wall_s")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=os.path.expanduser("~/sigil_bench3"))
    ap.add_argument("--seeds", default="s1,s2,s3")
    a = ap.parse_args()
    seeds = [s.strip() for s in a.seeds.split(",") if s.strip()]
    L = ["# 进化 benchmark 第 3 轮报告(两阶段 + held-out 泛化)", ""]

    # ---------- Phase A: evolution ----------
    L.append("## Phase A — 进化期(T1x/T4/T2 × epochs)")
    L.append("")
    for s in seeds:
        rows = load(a.base, "sigil", s) or []
        ph_a = [r for r in rows if r.get("task") in ("T1x", "T4", "T2")]
        if not ph_a:
            L.append(f"- seed {s}: 无 Phase A 数据")
            continue
        eps = sorted({r.get("epoch", 0) for r in ph_a})
        L.append(f"### seed {s}")
        L.append("")
        L.append("| epoch | 通过 | tokens | steps | wall_s | 新增资产 |")
        L.append("|---|---|---|---|---|---|")
        for e in eps:
            er = [r for r in ph_a if r.get("epoch") == e]
            hit = sum(1 for r in er if r.get("verdict") in ("pass", "output-pass"))
            tok = sum((r.get("prompt_tokens") or 0) + (r.get("completion_tokens") or 0) for r in er)
            stp = sum(r.get("steps") or 0 for r in er)
            wall = round(sum(r.get("wall_s") or 0 for r in er), 1)
            add = sum(len((r.get("delta") or {}).get("added", [])) for r in er)
            L.append(f"| ep{e} | {hit}/{len(er)} | {tok} | {stp} | {wall} | +{add} |")
        L.append("")

    # ---------- Phase B: held-out ----------
    L.append("## Phase B — held-out 泛化考卷(H1/H2,冻结运行)")
    L.append("")
    L.append("| seed | 组 | H1 | H2 | tokens | steps | wall_s |")
    L.append("|---|---|---|---|---|---|---|")
    agg = {"sigil": [], "dsh": []}
    for s in seeds:
        for grp in ("sigil", "dsh"):
            rows = load(a.base, grp, s) or []
            hb = [r for r in rows if r.get("task") in ("H1", "H2")
                  and (grp == "dsh" or r.get("epoch") == max(
                      (x.get("epoch", 0) for x in rows if x.get("task") in ("H1", "H2")), default=0))]
            if not hb:
                L.append(f"| {s} | {grp} | — | — | — | — | — |")
                continue
            v = {r["task"]: r.get("verdict") for r in hb}
            tok = sum((r.get("prompt_tokens") or 0) + (r.get("completion_tokens") or 0) for r in hb)
            stp = sum(r.get("steps") or 0 for r in hb)
            wall = round(sum(r.get("wall_s") or 0 for r in hb), 1)
            # Protocol violation check: under SIGIL_FREEZE the asset dirs must
            # stay byte-identical. Any non-empty delta (e.g. the model wrote a
            # .lua straight into tools/ via bash to bypass the frozen tools)
            # is flagged — those scores measure cheating, not transfer.
            d = [x for r in hb for x in (
                (r.get("delta") or {}).get("added", [])
                + (r.get("delta") or {}).get("changed", [])
                + (r.get("delta") or {}).get("removed", []))]
            viol = f" ⚠VIOLATION×{len(d)}" if d else ""
            L.append(f"| {s} | {grp} | {v.get('H1','-')} | {v.get('H2','-')} | {tok} | {stp} | {wall} |{viol}")
            agg[grp].append((tok, stp, wall, sum(1 for r in hb if r.get("verdict") in ("pass", "output-pass"))))

    # ---------- headline ----------
    L.append("")
    L.append("## 头条指标: held-out cost-per-pass(sigil 冻结 vs dsh)")
    L.append("")
    def cpp(group):
        tot_tok = sum(x[0] for x in agg[group])
        tot_pass = sum(x[3] for x in agg[group])
        return (tot_tok / tot_pass) if tot_pass else float("nan"), tot_pass
    sv, sp_ = cpp("sigil")
    dv, dp = cpp("dsh")
    L.append(f"- sigil: {sp_} passes, {sv:.0f} tokens/pass")
    L.append(f"- dsh:   {dp} passes, {dv:.0f} tokens/pass")
    if dp and sp_ and dv == dv and sv == sv:
        L.append(f"- 比值 sigil/dsh = {sv/dv:.2f}（<1 = 进化面在未见任务上更省；token 口径 dsh 缺失时以 wall_s 报告）")
    L.append("")
    out = "\n".join(L)
    os.makedirs(a.base, exist_ok=True)
    open(os.path.join(a.base, "REPORT3.md"), "w", encoding="utf-8").write(out)
    print(out)

if __name__ == "__main__":
    main()
