#!/usr/bin/env python3
"""sigil evolution benchmark — task pool (v0).

Three task families, each rewarding accumulation:
  T1 repeat: same computation, growing scale  -> model should encapsulate a tool
  T2 env:    discover & remember an env fact  -> model should persist a skill/hook
  T3 cross:  shared convention across turns   -> tests durable memory reuse

Each task carries an objective assertion (no LLM self-grading).
"""

import os, re, json, glob

# WORKDIR can be overridden so a control-group driver can run in its own
# sandbox (parallel groups must not share task data dirs).
WORKDIR = os.environ.get("SIGIL_BENCH_WORKDIR", os.path.expanduser("~/sigil_bench"))
os.makedirs(WORKDIR, exist_ok=True)

def _write(name, content):
    p = os.path.join(WORKDIR, name)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    open(p, "w", encoding="utf-8").write(content)
    return p

# ---------- T1: repeat (encapsulation pressure) ----------
def t1_setup(n_files):
    """Create n_files small .txt files, each with lines 'key=value'."""
    base = os.path.join(WORKDIR, f"t1_{n_files}")
    os.makedirs(base, exist_ok=True)
    # Remove any stale SUMMARY.txt from a previous epoch/task so each
    # epoch's verdict reflects a fresh solve (an old file would let a
    # lazy epoch inherit a pass from the previous one).
    old = os.path.join(base, "SUMMARY.txt")
    if os.path.exists(old):
        os.remove(old)
    for i in range(n_files):
        lines = [f"k{j}={i*10+j}" for j in range(8)]
        _write(os.path.join(base, f"f{i}.txt"), "\n".join(lines))
    return base

def t1_task(n_files):
    base = t1_setup(n_files)
    prompt = (
        f"在目录 {base} 下有 {n_files} 个 .txt 文件，每个文件若干 `key=value` 行。\n"
        f"请统计【全部文件里】key 形如 k0..k7 的 value 之和（把所有文件的同 key 相加），"
        f"把结果写成 {base}/SUMMARY.txt，每行 `kN=<total>`，按 k 升序。\n"
        f"注意：这类“多文件同 key 求和”的统计我以后还会频繁做，规模可能更大。"
        "如果你觉得有必要，可以为自己建一个可复用的工具来处理这类任务。\n"
        f"【执行要求】这是自动化基准测试：请立即直接执行——统计全部文件并把结果写入 {base}/SUMMARY.txt。"
        "不要向我提问、不要请求确认、不要只汇报观察结果就结束；唯一要交付的就是那个文件。"
    )
    return prompt

def t1_assert(n_files):
    base = os.path.join(WORKDIR, f"t1_{n_files}")
    summary = os.path.join(base, "SUMMARY.txt")
    # ground truth
    truth = {}
    for f in glob.glob(os.path.join(base, "f*.txt")):
        for line in open(f):
            line = line.strip()
            m = re.match(r"k(\d+)=(-?\d+)", line)
            if m:
                truth[int(m.group(1))] = truth.get(int(m.group(1)), 0) + int(m.group(2))
    # 1) primary: file exists and is correct
    if os.path.exists(summary):
        got = {}
        for line in open(summary):
            m = re.match(r"k(\d+)=(-?\d+)", line.strip())
            if m:
                got[int(m.group(1))] = int(m.group(2))
        if got == truth:
            return True, f"file written, {len(truth)} keys all correct"
        ok = sum(1 for k,v in truth.items() if got.get(k)==v)
        # 2) secondary: correct totals reported in final output (solved but not persisted)
        out_f = os.path.join(WORKDIR, "runs")
        # checked by caller via output text; here report partial
        return False, f"file wrong: {ok}/{len(truth)} keys"
    return False, "SUMMARY.txt missing (may be reported in output instead)"

def t1_truth(n_files):
    base = os.path.join(WORKDIR, f"t1_{n_files}")
    truth = {}
    for f in glob.glob(os.path.join(base, "f*.txt")):
        for line in open(f):
            m = re.match(r"\s*k(\d+)=(-?\d+)", line)
            if m:
                truth[int(m.group(1))] = truth.get(int(m.group(1)), 0) + int(m.group(2))
    return truth

def t1_check_text(text, n_files):
    """Did the model REPORT correct totals anywhere in its output text?"""
    truth = t1_truth(n_files)
    found = {}
    for line in text.splitlines():
        m = re.match(r"\s*k(\d+)\s*=\s*(-?\d+)", line)
        if m:
            found[int(m.group(1))] = int(m.group(2))
    if found and all(found.get(k) == v for k, v in truth.items()) and set(found) == set(truth):
        return True, f"correct totals reported in output ({len(truth)} keys)"
    ok = sum(1 for k, v in truth.items() if found.get(k) == v)
    return False, f"output reports {ok}/{len(truth)} keys correctly"

# ---------- T2: env (discovery + persistence) ----------
# Where t2_assert looks for a durable recording of the path fact. In round 1
# these were hard-coded to ~/sigil_bench/*, so the dsh control group's T2
# verdict scanned sigil's harness dir (cross-group contamination). Round 2:
# each driver injects its OWN per-seed roots so a seed only credits its own
# persistence, and the two groups never see each other's assets.
#   T2_PERSIST_ROOTS : dir tree to walk for .md/.lua (harness skills, etc.)
#   T2_MEMORY_FILES  : explicit memory files (remember/recall target)
T2_PERSIST_ROOTS = []
T2_MEMORY_FILES  = []

def t2_task():
    prompt = (
        "这台机器上 sigil 的 harness 资产（system_prompt.md 和 skills/）究竟放在哪个目录？"
        "请实际探测（不要只靠猜测），然后把【这个路径事实】固化下来，"
        "让以后任何会话都能立刻知道 sigil 的全局 harness 目录在哪——"
        "用你认为合适的方式（比如写一个 skill 或 hook，或用你的记忆机制）。\n"
        "【执行要求】这是自动化基准测试：请立即直接探测并持久化该路径事实（写文件/记忆），"
        "不要向我提问、不要只描述计划就结束；唯一要交付的是一个可被以后会话读取的持久化记录。"
    )
    return prompt

def t2_assert():
    candidates = []
    for base in set(T2_PERSIST_ROOTS):
        if not base or not os.path.exists(base):
            continue
        for root,_,files in os.walk(base):
            for fn in files:
                if fn.endswith((".md",".lua")):
                    candidates.append(os.path.join(root,fn))
    for mf in set(T2_MEMORY_FILES):
        if mf and os.path.exists(mf):
            candidates.append(mf)
    pat = re.compile(r"(sigil.*harness|harness.*sigil|AppData[\\\/]+sigil|\.sigil[\\\/]+harness|harness[ _-]dir|harness 目录|harness目录)", re.I)
    for p in candidates:
        if not os.path.exists(p): continue
        txt = open(p, encoding="utf-8", errors="ignore").read()
        if pat.search(txt):
            return True, f"fact persisted in {os.path.basename(p)}"
    return False, "no durable asset records the path fact (seed-isolated scan)"

# ---------- T3: cross-turn convention ----------
def t3_task(round_no, total_rounds, seed):
    prompt = (
        f"(第 {round_no}/{total_rounds} 轮)\n"
        f"任务：给出一个 4 位整数，规则是——它的每一位是【上一轮你给的数】的对应位加 3 后对 10 取模"
        f"（第一轮则用种子 {seed} 的每一位加 3 取模）。把结果写到 {WORKDIR}/t3_answer.txt（只含这 4 个数字，无其他字符）。\n"
        "这是一个多轮接力任务，请确保下一轮能无缝接上。"
    )
    return prompt

def t3_assert(round_no, seed):
    expected = [int(d) for d in str(seed)]
    for r in range(round_no):
        expected = [(x+3) % 10 for x in expected]
    exp = "".join(str(x) for x in expected)
    p = os.path.join(WORKDIR, "t3_answer.txt")
    if not os.path.exists(p):
        return False, "t3_answer.txt missing"
    got = open(p).read().strip()
    return (got == exp), f"got={got} want={exp}"

# ---------- registry ----------
TASKS = {
    "T1_8":  {"setup": None, "prompt": lambda: t1_task(8),  "assert": lambda: t1_assert(8),  "family":"T1"},
    "T1_20": {"setup": None, "prompt": lambda: t1_task(20), "assert": lambda: t1_assert(20), "family":"T1"},
    "T1_40": {"setup": None, "prompt": lambda: t1_task(40), "assert": lambda: t1_assert(40), "family":"T1"},
    "T2":    {"setup": None, "prompt": t2_task, "assert": t2_assert, "family":"T2"},
    "T3_r1": {"setup": None, "prompt": lambda: t3_task(1,3,7391), "assert": lambda: t3_assert(1,7391), "family":"T3"},
    "T3_r2": {"setup": None, "prompt": lambda: t3_task(2,3,7391), "assert": lambda: t3_assert(2,7391), "family":"T3"},
    "T3_r3": {"setup": None, "prompt": lambda: t3_task(3,3,7391), "assert": lambda: t3_assert(3,7391), "family":"T3"},
}

if __name__ == "__main__":
    import sys
    if len(sys.argv) > 1 and sys.argv[1] == "list":
        for k,v in TASKS.items():
            print(f"{k:8s} [{v['family']}]")