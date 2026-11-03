//! plan: dynamic-workflow plan language — a LuaJIT sandbox port of Fareed
//! Khan's "Building Dynamic Workflows" restricted-Python planner (2026-11).
//!
//! The design bet, validated by the ~/dw-poc PoC: the original's expensive
//! AST-whitelist checker is unnecessary when the plan VM is *constructed*
//! closed — globals contain only an injected whitelist plus the `kit` table,
//! and no io/os/ffi/package/debug stdlib is ever opened. Name closure by
//! construction beats name closure by inspection.
//!
//! Pipeline (mirrors the original's six steps):
//!   static gate  →  dry run on a blind stand-in (instant price estimate)
//!   →  approval gate (auto ≤ min(40, budget/3), held ≤ budget, refused >)
//!   →  run under a budget with a wall-clock runaway hook and a fan-out pool.
//!
//! Original's rule kept verbatim: the blind stand-in must see the SAME budget
//! the real run will see, or the price estimate is meaningless.

use mlua::{Lua, LuaOptions, StdLib, Table, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_LINES: usize = 80;
pub const MAX_CHARS: usize = 6_000;
pub const AUTO_APPROVE_CREDITS: f64 = 40.0;
pub const AUTO_APPROVE_SHARE: f64 = 1.0 / 3.0;
pub const DEFAULT_PRICE_PER_CALL: f64 = 0.2;

/// Words a plan may not contain (error boundary, not a security boundary —
/// the sandbox is the security boundary; this gate catches model mistakes
/// early with a line number the lead can act on, like the original's
/// "WHY_NOT" messages).
const BANNED_WORDS: &[&str] = &[
    "while",
    "repeat",
    "require",
    "loadstring",
    "loadfile",
    "dofile",
    "load(",
    "os.",
    "io.",
    "ffi",
    "debug.",
    "collectgarbage",
    "getfenv",
    "setfenv",
    "rawget",
    "rawset",
    "rawequal",
    "rawlen",
    "_G",
    "string.dump",
];

// ---------------------------------------------------------------------------
// Budget — reserve/settle accounting with an explicit refusal (never a silent
// skip: an over-cap call is an error the plan and the human both see).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Budget {
    cap: Mutex<f64>,
    inner: Mutex<(f64, f64)>, // (spent, reserved)
}

impl Budget {
    pub fn new(cap: f64) -> Self {
        Budget {
            cap: Mutex::new(cap),
            inner: Mutex::new((0.0, 0.0)),
        }
    }
    /// Reserve a call's price. This port refuses instead of pausing (there is
    /// no human attached to a plan run to raise the cap mid-flight; `raise_to`
    /// exists for callers that wrap this in an interactive loop).
    pub fn admit(&self, cost: f64) -> Result<(), String> {
        let (spent, reserved) = *self.inner.lock().unwrap();
        let cap = *self.cap.lock().unwrap();
        if spent + reserved + cost > cap + 1e-9 {
            return Err(format!(
                "BUDGET_REFUSED: {:.1} in flight/spent + {:.1} would exceed cap {:.1}",
                spent + reserved,
                cost,
                cap
            ));
        }
        self.inner.lock().unwrap().1 += cost;
        Ok(())
    }
    /// A reservation becomes spending — or is released when the call failed
    /// and is not billed.
    pub fn settle(&self, reserved: f64, actual: f64) {
        let mut g = self.inner.lock().unwrap();
        g.1 -= reserved;
        g.0 += actual;
    }
    pub fn raise_to(&self, cap: f64) {
        *self.cap.lock().unwrap() = cap;
    }
    pub fn spent(&self) -> f64 {
        self.inner.lock().unwrap().0
    }
    pub fn remaining(&self) -> f64 {
        let (spent, reserved) = *self.inner.lock().unwrap();
        *self.cap.lock().unwrap() - spent - reserved
    }
    pub fn cap(&self) -> f64 {
        *self.cap.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Static gate
// ---------------------------------------------------------------------------

fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    let w = word.as_bytes();
    let wlen = w.len();
    if wlen == 0 || bytes.len() < wlen {
        return false;
    }
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    // "while" must be delimited by non-word bytes (or the word itself
    // contains '(' like "load(", then only the left side is checked).
    let right_paren = word.ends_with('(');
    for start in 0..=(bytes.len() - wlen) {
        if &bytes[start..start + wlen] != w {
            continue;
        }
        if start > 0 && is_word(bytes[start - 1]) {
            continue;
        }
        if !right_paren {
            let after = start + wlen;
            if after < bytes.len() && is_word(bytes[after]) {
                continue;
            }
        }
        return true;
    }
    false
}

/// Every problem with a plan, by line. An empty list means it may go on.
pub fn static_gate(src: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    if lines.len() > MAX_LINES {
        problems.push(format!(
            "line 1: the program has {} lines, the limit is {}",
            lines.len(),
            MAX_LINES
        ));
    }
    if src.len() > MAX_CHARS {
        problems.push(format!(
            "line 1: the program has {} characters, the limit is {}",
            src.len(),
            MAX_CHARS
        ));
    }
    for (i, line) in lines.iter().enumerate() {
        for word in BANNED_WORDS {
            if contains_word(line, word) {
                problems.push(format!(
                    "line {}: '{}' is not allowed in a plan (use a bounded for loop; a plan has nothing to require and no OS to touch)",
                    i + 1,
                    word
                ));
            }
        }
    }
    if !src.contains("return") {
        problems.push("line 1: the program never returns a result".to_string());
    }
    problems
}

// ---------------------------------------------------------------------------
// Plan VM — constructed-closed sandbox
// ---------------------------------------------------------------------------

fn plan_vm() -> Lua {
    // SAFETY: safe mode refuses stdlib combinations; we open only the four
    // pure-computation libraries. No ffi, no io/os, no package, no jit lib.
    unsafe {
        Lua::unsafe_new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::BIT,
            LuaOptions::default(),
        )
    }
}

fn inject_whitelist(lua: &Lua, globals: &Table) -> mlua::Result<()> {
    // set_globals swaps the whole table, which orphans the stdlib library
    // tables too (measured in the PoC — stricter than expected). Copy the
    // pure-computation ones back in.
    let old = lua.globals();
    for name in ["table", "string", "math", "bit"] {
        let v: Value = old.get(name)?;
        if let Value::Table(t) = v {
            globals.set(name, t)?;
        }
    }
    globals.set(
        "type",
        lua.create_function(|_, v: Value| {
            Ok(match v {
                Value::Nil => "nil".to_string(),
                Value::Boolean(_) => "boolean".to_string(),
                Value::LightUserData(_) => "lightuserdata".to_string(),
                Value::Integer(_) | Value::Number(_) => "number".to_string(),
                Value::String(_) => "string".to_string(),
                Value::Table(_) => "table".to_string(),
                Value::Function(_) => "function".to_string(),
                Value::Thread(_) => "thread".to_string(),
                _ => "userdata".to_string(),
            })
        })?,
    )?;
    globals.set(
        "tonumber",
        lua.create_function(|_, v: Value| {
            Ok(match v {
                Value::String(s) => match s.to_str()?.trim().parse::<f64>() {
                    Ok(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::Integer(f as i64),
                    Ok(f) => Value::Number(f),
                    Err(_) => Value::Nil,
                },
                other => other,
            })
        })?,
    )?;
    globals.set(
        "tostring",
        lua.create_function(|_, v: Value| {
            Ok(match v {
                Value::String(s) => s.to_str()?.to_string(),
                Value::Integer(i) => i.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Boolean(b) => b.to_string(),
                _ => v.type_name().to_string(),
            })
        })?,
    )?;
    // pairs(t) -> {key, key, ...}: plans iterate with #t + indexing, which is
    // all a plan needs and cannot be turned into a traversal weapon.
    globals.set(
        "pairs",
        lua.create_function(|lua, t: Table| {
            let keys = lua.create_table()?;
            let mut i = 1;
            for kv in t.pairs::<Value, Value>() {
                let (k, _) = kv?;
                keys.set(i, k)?;
                i += 1;
            }
            Ok(keys)
        })?,
    )?;
    Ok(())
}

/// Wall-clock runaway hook. The original detects runaway plans at the agent
/// limit (twice the cap); an instruction-count hook kills a busy loop before
/// the first worker is even requested.
fn arm_runaway_hook(lua: &Lua, wall: Duration) {
    let start = Instant::now();
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(200_000),
        move |_lua, _debug| {
            if start.elapsed() > wall {
                Err(mlua::Error::RuntimeError(
                    "PLAN_RUNAWAY: wall budget exceeded".to_string(),
                ))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    )
    .expect("set_hook");
}

fn plan_result_string(lua: &Lua) -> mlua::Result<String> {
    // re-stringify through the whitelist tostring, the same path a plan gets
    let f: mlua::Function = lua.globals().get("tostring")?;
    Ok(f.call::<String>(lua.load("__plan_result").eval::<Value>()?)?)
}

// ---------------------------------------------------------------------------
// kit — blind stand-in (dry run) and real fan-out (run)
// ---------------------------------------------------------------------------

/// Build the blind kit: map_scan answers instantly for free and leaves only
/// a call count behind (the original's `blind()`).
fn blind_kit(lua: &Lua, budget: &Arc<Budget>) -> mlua::Result<Table> {
    let kit = lua.create_table()?;
    let shards = lua.create_table()?;
    for i in 1..=14i64 {
        shards.set(i, i)?;
    }
    kit.set("shards", shards)?;
    let calls = Arc::new(Mutex::new(0u32));
    let budget = Arc::clone(budget);
    let calls2 = Arc::clone(&calls);
    kit.set("__calls", 0u32)?;
    kit.set(
        "map_scan",
        lua.create_function(move |lua, (ids, _lens): (Table, String)| {
            let n = ids.len()?;
            *calls2.lock().unwrap() += n as u32;
            let kit: Table = lua.globals().get("kit")?;
            kit.set("__calls", *calls2.lock().unwrap())?;
            let out = lua.create_table()?;
            for i in 1..=n {
                out.set(i, "blind")?;
            }
            Ok(out)
        })?,
    )?;
    let budget2 = Arc::clone(&budget);
    kit.set(
        "budget",
        lua.create_function(move |_, ()| Ok(budget2.remaining().max(0.0)))?,
    )?;
    kit.set("log", lua.create_function(|_, _msg: String| Ok(()))?)?;
    Ok(kit)
}

/// A scan worker: given (shard_id, lens) return that worker's finding ack.
/// Real deployments inject an LLM call here; tests inject a mock.
pub type ScanImpl = Arc<dyn Fn(i64, String) -> String + Send + Sync>;

/// Build the real kit: map_scan fans one job per shard id into the pool,
/// waits for all of them (barrier, like the original's `parallel()`), and
/// returns results in shard order.
fn real_kit(
    lua: &Lua,
    budget: &Arc<Budget>,
    price: f64,
    scan: ScanImpl,
    pool: &crate::tools::plan_pool::Pool,
    calls: &Arc<Mutex<u32>>,
) -> mlua::Result<Table> {
    let kit = lua.create_table()?;
    let shards = lua.create_table()?;
    for i in 1..=14i64 {
        shards.set(i, i)?;
    }
    kit.set("shards", shards)?;
    let budget2 = Arc::clone(budget);
    kit.set(
        "budget",
        lua.create_function(move |_, ()| Ok(budget2.remaining().max(0.0)))?,
    )?;
    let budget3 = Arc::clone(budget);
    let scan2 = Arc::clone(&scan);
    let pool2 = pool.clone();
    let calls2 = Arc::clone(calls);
    kit.set(
        "map_scan",
        lua.create_function(move |lua, (ids, lens): (Table, String)| {
            let n = ids.len()?;
            let mut jobs = Vec::with_capacity(n as usize);
            for i in 1..=n {
                let id: i64 = ids.get(i)?;
                jobs.push((id, lens.clone()));
            }
            *calls2.lock().unwrap() += jobs.len() as u32;
            // reserve the whole batch up front — one explicit refusal beats
            // a half-funded scan phase
            budget3
                .admit(jobs.len() as f64 * price)
                .map_err(mlua::Error::external)?;
            let results = pool2.map(jobs.clone(), |(id, lens)| {
                let ack = scan2(id, lens);
                format!("shard#{}:{}:{}", id, ack, ack.len())
            });
            budget3.settle(jobs.len() as f64 * price, jobs.len() as f64 * price);
            let out = lua.create_table()?;
            for (i, r) in results.into_iter().enumerate() {
                out.set(i + 1, r)?;
            }
            Ok(out)
        })?,
    )?;
    let budget4 = Arc::clone(budget);
    kit.set(
        "log",
        lua.create_function(move |_, msg: String| {
            eprintln!("[plan] {}", msg);
            let _ = &budget4;
            Ok(())
        })?,
    )?;
    Ok(kit)
}

// ---------------------------------------------------------------------------
// Execute a plan source on a prepared VM
// ---------------------------------------------------------------------------

fn execute(lua: &Lua, src: &str, wall: Duration) -> Result<String, String> {
    arm_runaway_hook(lua, wall);
    let chunk = lua
        .load(format!(
            "local __plan_result = function()\n{}\nend\nreturn __plan_result()",
            src
        ))
        .set_name("plan");
    let v: Value = chunk.eval().map_err(|e| format!("plan error: {e}"))?;
    lua.globals()
        .set("__plan_result", v)
        .map_err(|e| format!("plan error: {e}"))?;
    let s = plan_result_string(lua).map_err(|e| format!("plan error: {e}"))?;
    lua.remove_hook();
    Ok(s)
}

// ---------------------------------------------------------------------------
// Public API: dry run, approval, run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Estimate {
    pub calls: u32,
    pub credits: f64,
}

/// Price a plan with the blind stand-in. The stand-in sees the same budget
/// cap the real run will see (original's hard-won rule).
pub fn dry_run(
    src: &str,
    budget_cap: f64,
    price: f64,
    wall: Duration,
) -> Result<Estimate, Vec<String>> {
    let problems = static_gate(src);
    if !problems.is_empty() {
        return Err(problems);
    }
    let budget = Arc::new(Budget::new(budget_cap));
    let lua = plan_vm();
    let globals = lua
        .create_table()
        .map_err(|e| vec![format!("vm error: {e}")])?;
    inject_whitelist(&lua, &globals).map_err(|e| vec![format!("vm error: {e}")])?;
    let kit = blind_kit(&lua, &budget).map_err(|e| vec![format!("vm error: {e}")])?;
    globals
        .set("kit", kit)
        .map_err(|e| vec![format!("vm error: {e}")])?;
    lua.set_globals(globals)
        .map_err(|e| vec![format!("vm error: {e}")])?;
    // The blind kit counts calls internally through the plan run; recover it
    // from the kit table afterwards.
    let calls: u32 = match execute(&lua, src, wall) {
        Ok(_) => {
            let kit: Table = lua
                .globals()
                .get("kit")
                .map_err(|e| vec![format!("vm error: {e}")])?;
            // calls counter lives in the closure; re-derive by scanning: the
            // blind kit stored its counter on the table itself.
            let v: Value = kit
                .get("__calls")
                .map_err(|e| vec![format!("vm error: {e}")])?;
            match v {
                Value::Integer(i) => i as u32,
                _ => 0,
            }
        }
        Err(e) if e.contains("PLAN_RUNAWAY") => {
            return Err(vec![format!(
                "line 1: the dry run exceeded its wall budget, so the plan probably loops without end"
            )]);
        }
        Err(e) => return Err(vec![e]),
    };
    Ok(Estimate {
        calls,
        credits: calls as f64 * price,
    })
}

/// Pre-written policy, decided before any plan is priced.
pub fn approve(est: &Estimate, budget_cap: f64) -> (&'static str, String) {
    let limit = AUTO_APPROVE_CREDITS.min(AUTO_APPROVE_SHARE * budget_cap);
    if est.credits > budget_cap {
        (
            "refused",
            format!(
                "estimated {:.1} credits is more than the whole budget of {:.0}",
                est.credits, budget_cap
            ),
        )
    } else if est.credits <= limit {
        (
            "approved",
            format!(
                "estimated {:.1} credits is under the automatic limit of {:.1}",
                est.credits, limit
            ),
        )
    } else {
        (
            "held",
            format!(
                "estimated {:.1} credits is over the automatic limit of {:.1}, a person must approve it",
                est.credits, limit
            ),
        )
    }
}

#[derive(Debug)]
pub struct RunStats {
    pub calls: u32,
    pub credits_spent: f64,
    pub result: String,
    pub wall: Duration,
}

/// Run an approved plan for real: fan-out pool, shared budget, runaway hook.
pub fn run_plan(
    src: &str,
    budget_cap: f64,
    price: f64,
    wall: Duration,
    scan: ScanImpl,
    concurrency: usize,
) -> Result<RunStats, Vec<String>> {
    let problems = static_gate(src);
    if !problems.is_empty() {
        return Err(problems);
    }
    let budget = Arc::new(Budget::new(budget_cap));
    let lua = plan_vm();
    let globals = lua
        .create_table()
        .map_err(|e| vec![format!("vm error: {e}")])?;
    inject_whitelist(&lua, &globals).map_err(|e| vec![format!("vm error: {e}")])?;
    let pool = crate::tools::plan_pool::Pool::new(concurrency);
    let calls = Arc::new(Mutex::new(0u32));
    let kit = real_kit(&lua, &budget, price, scan, &pool, &calls)
        .map_err(|e| vec![format!("vm error: {e}")])?;
    globals
        .set("kit", kit)
        .map_err(|e| vec![format!("vm error: {e}")])?;
    lua.set_globals(globals)
        .map_err(|e| vec![format!("vm error: {e}")])?;
    let start = Instant::now();
    let result = execute(&lua, src, wall).map_err(|e| vec![e])?;
    let stats = RunStats {
        calls: *calls.lock().unwrap(),
        credits_spent: budget.spent(),
        result,
        wall: start.elapsed(),
    };
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"
local ids = {}
for i = 1, #kit.shards do ids[#ids+1] = i end
local lenses = {"general", "logic", "boundaries", "state"}
local all = {}
for r = 1, 3 do
  for li = 1, #lenses do
    local found = kit.map_scan(ids, lenses[li])
    for fi = 1, #found do all[#all+1] = found[fi] end
  end
end
return #all .. " findings from " .. #kit.shards .. " shards"
"#;

    #[test]
    fn gate_accepts_canonical_plan() {
        assert!(static_gate(PLAN).is_empty(), "{:?}", static_gate(PLAN));
    }

    #[test]
    fn gate_rejects_while_and_require() {
        let p = static_gate("local t = 0\nwhile true do t = t + 1 end\nreturn t");
        assert!(p.iter().any(|s| s.contains("'while'")), "{p:?}");
        let p = static_gate("require('os')\nreturn 1");
        assert!(p.iter().any(|s| s.contains("require")), "{p:?}");
        // 'while' inside a word must NOT trigger (whitelist not too strict)
        assert!(
            !static_gate("local worthwhile = 1\nreturn worthwhile")
                .iter()
                .any(|s| s.contains("'while'"))
        );
    }

    #[test]
    fn gate_rejects_length_and_missing_return() {
        let long = (0..81)
            .map(|i| format!("local v{i} = {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(static_gate(&long).iter().any(|s| s.contains("lines")));
        assert!(
            static_gate("local x = 1")
                .iter()
                .any(|s| s.contains("never returns"))
        );
    }

    #[test]
    fn sandbox_blocks_escape_probes() {
        let lua = plan_vm();
        let globals = lua.create_table().unwrap();
        inject_whitelist(&lua, &globals).unwrap();
        let bc = Arc::new(Mutex::new(0u32));
        let kit = blind_kit_counting(&lua, &bc);
        globals.set("kit", kit).unwrap();
        lua.set_globals(globals).unwrap();
        for probe in [
            "return os",
            "return io",
            "return require",
            "return loadstring",
            "return debug",
            "return package",
            "return _G",
            "return collectgarbage",
            "return getfenv and 1",
            "return rawget and 1",
        ] {
            let v: Value = lua.load(probe).eval().unwrap_or(Value::Nil);
            assert!(
                matches!(v, Value::Nil) || matches!(v, Value::Boolean(false)),
                "LEAK: {probe} -> {}",
                v.type_name()
            );
        }
        // whitelist is usable
        let t: String = lua.load("return type('x')").eval().unwrap();
        assert_eq!(t, "string");
        let ok: bool = lua
            .load("local t={3,1,2} table.sort(t) return t[1]==1")
            .eval()
            .unwrap();
        assert!(ok);
    }

    fn blind_kit_counting(lua: &Lua, calls: &Arc<Mutex<u32>>) -> Table {
        // like blind_kit but stores the counter on the table so tests can read it
        let kit = lua.create_table().unwrap();
        let shards = lua.create_table().unwrap();
        for i in 1..=14i64 {
            shards.set(i, i).unwrap();
        }
        kit.set("shards", shards).unwrap();
        let c2 = Arc::clone(calls);
        kit.set("__calls", 0u32).unwrap();
        kit.set(
            "map_scan",
            lua.create_function(move |lua, (ids, _lens): (Table, String)| {
                let n = ids.len()?;
                *c2.lock().unwrap() += n as u32;
                let out = lua.create_table()?;
                for i in 1..=n {
                    out.set(i, "blind").unwrap();
                }
                // mirror counter onto kit for dry_run recovery
                let kit: Table = lua.globals().get("kit")?;
                kit.set("__calls", *c2.lock().unwrap())?;
                Ok(out)
            })
            .unwrap(),
        )
        .unwrap();
        let b = Arc::new(Budget::new(150.0));
        kit.set(
            "budget",
            lua.create_function(move |_, ()| Ok(b.remaining().max(0.0)))
                .unwrap(),
        )
        .unwrap();
        kit.set("log", lua.create_function(|_, _m: String| Ok(())).unwrap())
            .unwrap();
        kit
    }

    #[test]
    fn dry_run_prices_canonical_plan() {
        // use the counting kit through the public path
        let est = dry_run(PLAN, 150.0, 0.2, Duration::from_secs(5)).expect("dry run");
        assert_eq!(est.calls, 168);
        assert!((est.credits - 33.6).abs() < 1e-9);
    }

    #[test]
    fn approval_gate_three_way() {
        let est = Estimate {
            calls: 168,
            credits: 33.6,
        };
        assert_eq!(approve(&est, 150.0).0, "approved"); // 33.6 <= min(40, 50)
        assert_eq!(approve(&est, 90.0).0, "held"); // limit = 30
        assert_eq!(approve(&est, 30.0).0, "refused"); // 33.6 > 30
    }

    #[test]
    fn runaway_hook_kills_busy_loop() {
        let lua = plan_vm();
        let globals = lua.create_table().unwrap();
        inject_whitelist(&lua, &globals).unwrap();
        lua.set_globals(globals).unwrap();
        let start = Instant::now();
        let r = execute(
            &lua,
            "local t = 0 for i = 1, 100000000000 do t = t + i end return t",
            Duration::from_millis(300),
        );
        assert!(r.unwrap_err().contains("PLAN_RUNAWAY"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn budget_admit_settle_refuse() {
        let b = Budget::new(1.0);
        b.admit(0.6).unwrap();
        assert!(b.admit(0.6).is_err()); // explicit refusal, no silent skip
        b.settle(0.6, 0.6);
        b.admit(0.4).unwrap();
        assert!((b.spent() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn run_plan_fans_out() {
        let scan: ScanImpl = Arc::new(|id, lens| format!("{}:{}", id, lens.len()));
        let stats = run_plan(PLAN, 150.0, 0.2, Duration::from_secs(30), scan, 8).expect("run");
        assert_eq!(stats.calls, 168);
        assert!((stats.credits_spent - 33.6).abs() < 1e-9);
        assert!(
            stats.result.contains("168 findings from 14 shards"),
            "{}",
            stats.result
        );
    }

    #[test]
    fn run_plan_refuses_over_budget() {
        // budget 10 credits < 33.6 needed: the batch admit must refuse loudly
        let scan: ScanImpl = Arc::new(|id, lens| format!("{}:{}", id, lens.len()));
        let err = run_plan(PLAN, 10.0, 0.2, Duration::from_secs(10), scan, 4).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.contains("BUDGET_REFUSED") || e.contains("exceed")),
            "{err:?}"
        );
    }
}
