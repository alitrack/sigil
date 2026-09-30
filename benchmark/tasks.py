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
    # ---- round 3: harder training-family tasks (cost-per-pass metric) ----
    # NOTE: lambdas on purpose — this dict is evaluated before the round-3
    # function definitions below, so bare name references would NameError.
    "T1x":   {"setup": None, "prompt": lambda: t1x_task(), "assert": lambda: t1x_assert(), "family":"T1"},
    "T4":    {"setup": None, "prompt": lambda: t4_task(),   "assert": lambda: t4_assert(),   "family":"T4"},
    # ---- round 3: held-out generalization exam (NEVER shown in evolution) ----
    "H1":    {"setup": None, "prompt": lambda: h1_task(),   "assert": lambda: h1_assert(),   "family":"H"},
    "H2":    {"setup": None, "prompt": lambda: h2_task(),   "assert": lambda: h2_assert(),   "family":"H"},
}

# ======================= round 3 additions =======================
# Design (evolution-benchmark.md §11): pass-rate saturated in round 2, so the
# round-3 headline metric is cost-per-pass (tokens/steps/wall-time per solved
# task). Task design principle: "expensive for a bare model, cheap with a
# good tool". All assertions remain objective (deterministic ground truth,
# no LLM grading). A reference solver + selftest below proves each assert()
# accepts a correct answer and rejects corrupted ones.

def _natural_key(s):
    """'k10' -> ('k', 10): natural-order sort key (k2 < k10)."""
    parts = re.split(r"(\d+)", s)
    return tuple((int(p), "") if p.isdigit() else (-1, p) for p in parts if p != "")

# ---------- T1x: dirty multi-dir sum (training family, harder) ----------
T1X_FILES = 200          # across nested subdirs
T1X_KEYS  = 12           # k0..k11 -> natural order actually differs from lex
T1X_DIRTY = 0.10         # ~10% malformed lines

def _t1x_rng(seed):
    """Deterministic LCG so every epoch/seed regenerates identical data."""
    x = seed
    while True:
        x = (1103515245 * x + 12345) % (2**31)
        yield x

def t1x_setup(seed=42):
    base = os.path.join(WORKDIR, "t1x")
    old = os.path.join(base, "SUMMARY.txt")
    if os.path.exists(old):
        os.remove(old)
    rng = _t1x_rng(seed)
    for i in range(T1X_FILES):
        d = os.path.join(base, f"sub{i % 7}", f"g{i % 3}")
        os.makedirs(d, exist_ok=True)
        lines = []
        for j in range(T1X_KEYS):
            r = next(rng)
            line = f"k{j}={r % 1000}"
            if r % 100 < T1X_DIRTY * 100:  # deterministic ~10% dirty
                line = next(rng) % 4 and f"k{j}=N/A" or f"kj=={r}"  # malformed
                if r % 7 == 0:
                    line = f"  k{j} = {r % 1000}"  # padded variant (VALID)
            lines.append(line)
        _write(os.path.join(d, f"f{i}.txt"), "\n".join(lines))
    return base

def _t1x_parse(base):
    """Ground-truth parser: strict `kN=<int>` only; whitespace/padding ok;
    malformed (missing '=', non-int) lines ignored."""
    truth = {}
    for f in glob.glob(os.path.join(base, "**", "f*.txt"), recursive=True):
        for line in open(f, encoding="utf-8", errors="ignore"):
            m = re.match(r"\s*k(\d+)\s*=\s*(-?\d+)\s*$", line)
            if m:
                k = int(m.group(1))
                truth[k] = truth.get(k, 0) + int(m.group(2))
    return truth

def t1x_task():
    base = t1x_setup()
    prompt = (
        f"目录 {base} 下（含多层子目录）散布着约 {T1X_FILES} 个 .txt 文件，每文件若干行，"
        f"合法行形如 `kN=整数`（N 为 0~{T1X_KEYS-1}，= 两侧可能有空格），"
        "但约有一成的行是畸形行（缺 =、值非整数等）——畸形行必须忽略，不得计入。\n"
        f"请把全部合法行按 key 求总和，写入 {base}/SUMMARY.txt，每行 `kN=<total>`，"
        "**key 顺序必须按自然序（natural order：k2 排在 k10 前面）**。\n"
        "这类脏数据多目录求和我以后还会大量做。如果你认为有必要，可以建可复用工具。\n"
        "【执行要求】自动化基准测试：立即直接执行，不要提问、不要只汇报计划；唯一交付是那个文件。"
    )
    return prompt

def t1x_assert():
    base = os.path.join(WORKDIR, "t1x")
    summary = os.path.join(base, "SUMMARY.txt")
    truth = _t1x_parse(base)
    if not os.path.exists(summary):
        return False, "SUMMARY.txt missing"
    lines = [l.strip() for l in open(summary, encoding="utf-8") if l.strip()]
    got_pairs = []
    for l in lines:
        m = re.match(r"k(\d+)=(-?\d+)$", l)
        if not m:
            return False, f"bad line format: {l[:30]!r}"
        got_pairs.append((f"k{m.group(1)}", int(m.group(2))))
    want = sorted(truth.items(), key=lambda kv: _natural_key(f"k{kv[0]}"))
    want_pairs = [(f"k{k}", v) for k, v in want]
    if got_pairs == want_pairs:
        return True, f"{len(want_pairs)} keys correct, natural order"
    bad = sum(1 for a, b in zip(got_pairs, want_pairs) if a != b)
    return False, f"mismatch: {bad} of {len(want_pairs)} entries differ (order or value)"

# ---------- T4: structured extraction to CSV (training family) ----------
T4_LINES = 120

def t4_setup(seed=99):
    base = os.path.join(WORKDIR, "t4")
    os.makedirs(base, exist_ok=True)
    for f in ("app.log",):
        p = os.path.join(base, f)
        if os.path.exists(p):
            os.remove(p)
    old = os.path.join(base, "events.csv")
    if os.path.exists(old):
        os.remove(old)
    rng = _t1x_rng(seed)
    rows = []
    noise = ["# comment line", "", "trace: gc pause 3ms", "[boot] starting"]
    for i in range(T4_LINES):
        r = next(rng)
        ts = f"2026-09-{1 + i % 28:02d} {r % 24:02d}:{(r // 7) % 60:02d}:{(r // 13) % 60:02d}"
        user = ["alice", "bob", "carol", "dave"][r % 4]
        action = ["login", "logout", "upload", "download", "delete"][r % 5]
        bytes_ = r % 100000
        rows.append(f"[{ts}] user={user} action={action} bytes={bytes_}")
    body = rows + [noise[next(rng) % len(noise)] for _ in range(15)]
    # interleave noise deterministically
    merged = []
    ri, ni = 0, 0
    for i in range(len(body)):
        if i % 9 == 8 and ni < 15:
            merged.append(body[len(rows) + ni]); ni += 1
        else:
            merged.append(rows[ri]); ri += 1
    _write(os.path.join(base, "app.log"), "\n".join(merged) + "\n")
    return base

def _t4_truth(base):
    rows = []
    for line in open(os.path.join(base, "app.log"), encoding="utf-8"):
        m = re.match(r"\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\] user=(\w+) action=(\w+) bytes=(\d+)", line)
        if m:
            rows.append(",".join(m.groups()))
    return rows

def t4_task():
    base = t4_setup()
    prompt = (
        f"日志文件 {base}/app.log 里混有噪声行（空行、注释、trace 等）。"
        "合法事件行格式为：`[YYYY-MM-DD hh:mm:ss] user=<名字> action=<动作> bytes=<整数>`。\n"
        f"请把全部合法事件抽取为 CSV 写入 {base}/events.csv：首行表头 `timestamp,user,action,bytes`，"
        "其后每事件一行，字段顺序同表头，保持日志原顺序，不加引号不加空格。\n"
        "半结构化日志抽取是我以后的高频任务，可考虑沉淀工具。\n"
        "【执行要求】自动化基准测试：立即直接执行，不要提问；唯一交付是 events.csv。"
    )
    return prompt

def t4_assert():
    base = os.path.join(WORKDIR, "t4")
    p = os.path.join(base, "events.csv")
    if not os.path.exists(p):
        return False, "events.csv missing"
    got = [l.strip() for l in open(p, encoding="utf-8") if l.strip()]
    want = ["timestamp,user,action,bytes"] + _t4_truth(base)
    if got == want:
        return True, f"{len(want)-1} events extracted in order"
    diff = sum(1 for a, b in zip(got, want) if a != b)
    return False, f"csv differs: {diff} lines (got {len(got)}, want {len(want)})"

# ---------- H1: JSONL -> aligned table (held-out) ----------
# Cold formatting rules (fully specified in prompt, nothing to "remember"):
# rewards pipeline experience, not direct reuse of any training tool.
def h1_setup(seed=7):
    base = os.path.join(WORKDIR, "h1")
    os.makedirs(base, exist_ok=True)
    for f in ("rows.jsonl", "TABLE.TXT"):
        p = os.path.join(base, f)
        if os.path.exists(p):
            os.remove(p)
    rng = _t1x_rng(seed)
    names = ["alpha", "beta_all", "gamma-2", "delta_x", "eps"]
    recs = []
    for i in range(30):
        r = next(rng)
        recs.append({"name": names[i % len(names)], "count": r % 10_000_000})
    _write(os.path.join(base, "rows.jsonl"),
           "\n".join(json.dumps(x, ensure_ascii=False) for x in recs) + "\n")
    return base

def _h1_thousands(n):
    return f"{n:,}"

def _h1_truth(base):
    recs = [json.loads(l) for l in open(os.path.join(base, "rows.jsonl"), encoding="utf-8") if l.strip()]
    w1 = max(len("name"), max(len(r["name"]) for r in recs))
    counts = [_h1_thousands(r["count"]) for r in recs]
    w2 = max(len("count"), max(len(c) for c in counts))
    lines = [f"{'name':<{w1}} | {'count':<{w2}}"]
    lines.append("-" * (w1 + w2 + 3))
    for r, c in zip(recs, counts):
        lines.append(f"{r['name']:<{w1}} | {c:<{w2}}")
    return "\n".join(lines) + "\n"

def h1_task():
    base = h1_setup()
    prompt = (
        f"读取 {base}/rows.jsonl（每行一个 JSON 对象，字段 name 和 count），"
        f"生成对齐文本表写入 {base}/TABLE.TXT，规则：\n"
        "1) 两列：name 左对齐、count 左对齐，列间用 ` | ` 分隔；\n"
        "2) 每列宽度 = max(表头宽度, 该列所有值宽度)，表头行就是 `name` 和 `count`；\n"
        "3) count 必须格式化为千分位（如 1234567 -> 1,234,567），再参与宽度计算；\n"
        "4) 表头下一行是整行连字符，长度 = 两列宽度之和+3；\n"
        "5) 保持输入顺序，文件以换行结尾。\n"
        "【执行要求】自动化基准测试：立即直接执行，不要提问；唯一交付是 TABLE.TXT。"
    )
    return prompt

def h1_assert():
    base = os.path.join(WORKDIR, "h1")
    p = os.path.join(base, "TABLE.TXT")
    if not os.path.exists(p):
        return False, "TABLE.TXT missing"
    got = open(p, encoding="utf-8").read()
    want = _h1_truth(base)
    if got == want:
        return True, "aligned table byte-exact"
    import difflib
    d = next(difflib.unified_diff(want.splitlines(), got.splitlines(), lineterm=""), None)
    return False, f"table differs (first diff: {d})"

# ---------- H2: invariant check across two dirs (held-out) ----------
def h2_setup(seed=55):
    base = os.path.join(WORKDIR, "h2")
    os.makedirs(os.path.join(base, "A"), exist_ok=True)
    os.makedirs(os.path.join(base, "B"), exist_ok=True)
    old = os.path.join(base, "REPORT.txt")
    if os.path.exists(old):
        os.remove(old)
    rng = _t1x_rng(seed)
    truth = []
    for i in range(12):  # record files r000..r011
        rid = f"r{i:03d}"
        va = {f"f{j}": next(rng) % 5000 for j in range(6)}
        vb = dict(va)
        if i % 4 == 1:  # value mismatch in f0
            vb["f0"] = va["f0"] + 1
            truth.append(f"{rid}:f0 value A={va['f0']} B={vb['f0']}")
        if i % 3 == 2:  # missing key in B
            del vb["f5"]
            truth.append(f"{rid}:f5 missing-in-B")
        la = [f"{k}={v}" for k, v in va.items()]
        lb = [f"{k}={v}" for k, v in vb.items()]
        _write(os.path.join(base, "A", f"{rid}.txt"), "\n".join(la) + "\n")
        _write(os.path.join(base, "B", f"{rid}.txt"), "\n".join(lb) + "\n")
    return base, sorted(truth)

def h2_task():
    base, _ = h2_setup()
    prompt = (
        f"目录 {base}/A 与 {base}/B 各有 12 个同名记录文件（r000.txt..r011.txt），"
        "每个文件是若干 `字段=整数` 行。正常情况两版应完全一致（字段集合与值）。\n"
        f"请逐文件比对，把所有不一致写成 {base}/REPORT.txt，每行一条，格式严格为：\n"
        "- 值不同：`<文件id>:<字段> value A=<值> B=<值>`\n"
        "- B 缺字段：`<文件id>:<字段> missing-in-B`\n"
        "（文件id 不含 .txt，如 r001；行按 文件id、字段 升序排列；无误报不得漏报。）\n"
        "【执行要求】自动化基准测试：立即直接执行，不要提问；唯一交付是 REPORT.txt。"
    )
    return prompt

def h2_assert():
    # Ground truth is recomputed from the on-disk A/B files themselves (the
    # same contract the model is given), so it never depends on setup state.
    base = os.path.join(WORKDIR, "h2")
    tr = []
    for i in range(12):
        rid = f"r{i:03d}"
        da = dict(re.findall(r"(f\d+)=(\d+)", open(os.path.join(base, "A", rid + ".txt")).read()))
        db = dict(re.findall(r"(f\d+)=(\d+)", open(os.path.join(base, "B", rid + ".txt")).read()))
        for k in sorted(set(da) | set(db), key=lambda x: int(x[1:])):
            if k not in db:
                tr.append(f"{rid}:{k} missing-in-B")
            elif da[k] != db[k]:
                tr.append(f"{rid}:{k} value A={da[k]} B={db[k]}")
    truth = sorted(tr)
    p = os.path.join(base, "REPORT.txt")
    if not os.path.exists(p):
        return False, "REPORT.txt missing"
    got = sorted(l.strip() for l in open(p, encoding="utf-8") if l.strip())
    if got == truth:
        return True, f"{len(truth)} inconsistencies, exact"
    miss = set(truth) - set(got)
    extra = set(got) - set(truth)
    return False, f"missing={len(miss)} false={len(extra)} (want {len(truth)})"

# ---------- selftest: prove asserts accept a reference solve ----------
def _selftest():
    """Reference solver writes correct artifacts; every round-3 assert must
    PASS against them (guards against broken ground truth)."""
    import tempfile, shutil
    global WORKDIR
    old_wd = WORKDIR
    tmp = tempfile.mkdtemp(prefix="sigil_bench_st_")
    WORKDIR = tmp
    try:
        # T1x reference solve
        t1x_setup()
        base = os.path.join(WORKDIR, "t1x")
        truth = _t1x_parse(base)
        ordered = sorted(truth.items(), key=lambda kv: _natural_key(f"k{kv[0]}"))
        open(os.path.join(base, "SUMMARY.txt"), "w").write(
            "\n".join(f"k{k}={v}" for k, v in ordered) + "\n")
        ok, why = t1x_assert()
        print(f"T1x selftest: {'PASS' if ok else 'FAIL'} ({why})")
        assert ok
        # T4 reference solve
        t4_setup()
        b4 = os.path.join(WORKDIR, "t4")
        open(os.path.join(b4, "events.csv"), "w").write(
            "timestamp,user,action,bytes\n" + "\n".join(_t4_truth(b4)) + "\n")
        ok, why = t4_assert()
        print(f"T4  selftest: {'PASS' if ok else 'FAIL'} ({why})")
        assert ok
        # H1 reference solve
        h1_setup()
        b1 = os.path.join(WORKDIR, "h1")
        open(os.path.join(b1, "TABLE.TXT"), "w", encoding="utf-8").write(_h1_truth(b1))
        ok, why = h1_assert()
        print(f"H1  selftest: {'PASS' if ok else 'FAIL'} ({why})")
        assert ok
        # H2 reference solve (from regenerated truth via file comparison)
        h2_setup()
        bh = os.path.join(WORKDIR, "h2")
        tr = []
        for i in range(12):
            rid = f"r{i:03d}"
            da = dict(re.findall(r"(f\d+)=(\d+)", open(os.path.join(bh, "A", rid + ".txt")).read()))
            db = dict(re.findall(r"(f\d+)=(\d+)", open(os.path.join(bh, "B", rid + ".txt")).read()))
            for k in sorted(set(da) | set(db), key=lambda x: int(x[1:])):
                if k not in db:
                    tr.append(f"{rid}:{k} missing-in-B")
                elif da[k] != db[k]:
                    tr.append(f"{rid}:{k} value A={da[k]} B={db[k]}")
        open(os.path.join(bh, "REPORT.txt"), "w").write("\n".join(sorted(tr)) + "\n")
        ok, why = h2_assert()
        print(f"H2  selftest: {'PASS' if ok else 'FAIL'} ({why})")
        assert ok
        # negative control: corrupted T1x answer must FAIL
        open(os.path.join(base, "SUMMARY.txt"), "w").write("k0=1\nk1=2\n")
        ok, why = t1x_assert()
        assert not ok
        print("T1x negative control: PASS (corrupt answer rejected)")
        print("ALL SELFTESTS PASS")
    finally:
        WORKDIR = old_wd
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    import sys
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        _selftest()
    elif len(sys.argv) > 1 and sys.argv[1] == "list":
        for k,v in TASKS.items():
            print(f"{k:8s} [{v['family']}]")