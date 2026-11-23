//! Tool registry: native tools + scripted LuaJIT tools + evolution tools
//! behind one lookup, so the agent loop and the model treat all identically.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::harness;
use crate::llm::ToolSpec;
use crate::lua::LuaRuntime;

pub mod acp;
pub mod ask;
pub mod bash;
pub mod delegate;
pub mod evolve;
pub mod files;
pub mod git;
pub mod goals_tool;
pub mod mcp;
pub mod memory;
#[cfg(feature = "plan")]
pub mod plan_pool;
pub mod scripted;
pub mod subagent;
pub mod versions;

#[cfg(test)]
mod scripted_tests;

#[cfg(test)]
mod evolve_hooks_tests;
#[cfg(test)]
mod evolve_tests;
/// What a tool returns to the agent loop.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
}

/// Execution status separate from text so protocol clients need not infer
/// failure from content that legitimately starts with `error:`.
pub struct ToolExecution {
    pub text: String,
    pub is_error: bool,
}

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput>;
    /// Cordis P2 (reactive coeffects → dependency injection): the names of
    /// OTHER tools this tool depends on. The registry resolves them at
    /// runtime and tracks a readiness flag; when a declared dependency
    /// registers (or a blocker unregisters), `recompute_readiness`
    /// reactively propagates the change to this tool and its dependents.
    ///
    /// Default is an empty list — a tool with no declared dependencies is
    /// always ready, which is exactly today's behavior, so every existing
    /// tool is unaffected. Only tools that genuinely depend on another
    /// tool (e.g. a self-authored Lua tool that `-- requires:` a
    /// still-pending MCP tool) override this.
    fn dependencies(&self) -> Vec<String> {
        vec![]
    }
    /// Cordis P3: is this tool's effect runtime-revertible (P1)?
    ///
    /// `Some(true)` = a call to this tool registers a runtime-revertible
    /// effect (P1) that the session log can roll back frame-by-frame.
    /// `None` = the tool does not participate in the P1 revertible-effect
    /// system (the default for every pre-existing tool) — not assessed,
    /// not a claim of irreversibility. Only `improve_harness` (when wired
    /// with a live `EffectTracker`) overrides this to `Some(true)`.
    fn revertible(&self) -> Option<bool> {
        None
    }
}

/// Unified registry, keyed by advertised name.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    /// Tools hot-registered mid-run by evolution tools, drained by the
    /// agent loop after each turn. `HotOp` carries the operation
    /// (register / unregister) so create, update and delete all work.
    hot_queue: Arc<Mutex<Vec<HotOp>>>,
    /// Cordis P2: shared, reactive dependency/readiness state. Recomputed
    /// on every register/unregister; read by the `tool_dependencies`
    /// introspection tool (which holds a clone of this same Arc) without
    /// needing the registry that registered it.
    deps: Arc<RwLock<DependencyState>>,
}

/// Cordis P2 (reactive coeffects): the dependency graph and per-tool
/// readiness for one registry, kept in its own `Arc<RwLock<..>>` so both
/// the registry and the introspection tool can share it without an
/// ownership cycle.
#[derive(Default, Clone, Debug)]
pub struct DependencyState {
    /// tool name -> the names it declared it depends on.
    pub deps: HashMap<String, Vec<String>>,
    /// tool name -> ready? (ready = every declared dependency is itself
    /// registered AND ready, computed transitively).
    pub ready: HashMap<String, bool>,
    /// tool name -> declared dependencies that are not currently
    /// registered (the actionable "what's missing" list).
    pub missing: HashMap<String, Vec<String>>,
}

impl DependencyState {
    /// All tools that directly depend on `name` (reverse edges), sorted
    /// for deterministic output.
    pub fn dependents_of(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .deps
            .iter()
            .filter(|(_, ds)| ds.iter().any(|d| d == name))
            .map(|(t, _)| t.clone())
            .collect();
        out.sort();
        out
    }
}

/// The tools whose readiness flipped after a register/unregister — the
/// "reactive coeffect" notification. `became_ready`: a (transitive)
/// dependency registered. `became_blocked`: one unregistered.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct ReadinessDelta {
    pub became_ready: Vec<String>,
    pub became_blocked: Vec<String>,
}

/// A queued evolution operation on a scripted tool.
#[derive(Debug, Clone)]
pub struct HotOp {
    pub name: String,
    pub path: Option<std::path::PathBuf>,
    /// true = register/refresh, false = unregister.
    pub register: bool,
}

impl HotOp {
    pub fn register(name: &str, path: std::path::PathBuf) -> Self {
        Self {
            name: name.to_string(),
            path: Some(path),
            register: true,
        }
    }
    pub fn unregister(name: &str) -> Self {
        Self {
            name: name.to_string(),
            path: None,
            register: false,
        }
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap clone for concurrent sessions: shares the Arc'd tools and
    /// the hot queue, giving each session its own registry bookkeeping.
    pub fn clone_shallow(&self) -> Self {
        self.clone()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> ReadinessDelta {
        let name = tool.name().to_string();
        // Re-registering the same name replaces the impl but must not
        // duplicate the order entry (a scripted tool may shadow a
        // native tool of the same name — e.g. a model-authored
        // mcp_resources.lua — without breaking the advertised list).
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
        // Cordis P2 (reactive coeffect): a registration can unblock tools
        // that depend on it (directly or transitively) — recompute
        // readiness and return the delta so the caller learns who just
        // became ready. (Existing callers use this as a `;` statement and
        // ignore the delta, so the return is backward-compatible.)
        self.recompute_readiness()
    }

    /// Remove a tool by name (idempotent). Used by evolution tools to
    /// delete scripted tools at runtime. Returns the reactive delta (the
    /// tools that just became blocked on its removal).
    pub fn unregister(&mut self, name: &str) -> ReadinessDelta {
        self.tools.remove(name);
        self.order.retain(|n| n != name);
        self.recompute_readiness()
    }

    /// Cordis P2: recompute the dependency graph + per-tool readiness from
    /// the tools currently registered, and return the reactive delta — the
    /// tools whose readiness just flipped. Called on every
    /// register/unregister so the graph and readiness stay current, and so
    /// the shared [`DependencyState`] is always fresh for the introspection
    /// tool. Read-only on the registry's tool map; writes only the shared
    /// deps state.
    pub fn recompute_readiness(&self) -> ReadinessDelta {
        // 1. Snapshot the old readiness so we can diff at the end.
        let old = self.deps.read().unwrap().ready.clone();

        // 2. Current declared deps for every registered tool.
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for (name, tool) in &self.tools {
            deps.insert(name.clone(), tool.dependencies());
        }
        let registered: std::collections::HashSet<&str> =
            self.tools.keys().map(String::as_str).collect();

        // 3. Transitively compute readiness with cycle detection. A tool is
        //    ready iff every declared dependency is registered AND itself
        //    ready. A dependency cycle can never be satisfied → not ready.
        //    Memoized DFS: 0=unvisited, 1=in-progress, 2=done.
        let mut ready: HashMap<String, bool> = HashMap::new();
        let mut state: HashMap<String, u8> = HashMap::new();
        for name in deps.keys() {
            let _ = Self::dfs_ready(name, &deps, &registered, &mut ready, &mut state);
        }

        // 4. Direct "what's missing" per tool: declared deps not registered.
        let mut missing: HashMap<String, Vec<String>> = HashMap::new();
        for (name, ds) in &deps {
            let m: Vec<String> = ds
                .iter()
                .filter(|d| !registered.contains(d.as_str()))
                .cloned()
                .collect();
            if !m.is_empty() {
                missing.insert(name.clone(), m);
            }
        }

        // 5. Diff readiness against the old snapshot → the reactive delta.
        // A "became_ready" coeffect is a tool that was ALREADY registered
        // (present in the old snapshot) and was blocked, and is now ready
        // because a dependency arrived. A brand-new tool arriving is the
        // registration event itself, not a reactive flip, so it is not
        // reported. Symmetrically, "became_blocked" is a previously-ready
        // tool that lost a dependency.
        let mut delta = ReadinessDelta::default();
        for (name, &r) in &ready {
            if r && old.get(name) == Some(&false) {
                delta.became_ready.push(name.clone());
            }
        }
        for (name, &was_ready) in &old {
            if !was_ready {
                continue;
            }
            match ready.get(name) {
                Some(&true) => {} // still ready: no change
                Some(&false) => {
                    delta.became_blocked.push(name.clone());
                }
                None => {} // removed: gone, not "blocked"
            }
        }
        delta.became_ready.sort();
        delta.became_blocked.sort();

        // 6. Publish the fresh state to the shared slot.
        *self.deps.write().unwrap() = DependencyState {
            deps,
            ready,
            missing,
        };
        delta
    }

    fn dfs_ready(
        name: &str,
        deps: &HashMap<String, Vec<String>>,
        registered: &std::collections::HashSet<&str>,
        ready: &mut HashMap<String, bool>,
        state: &mut HashMap<String, u8>,
    ) -> bool {
        match state.get(name).copied().unwrap_or(0) {
            2 => return ready[name], // done: memoized
            1 => return false,       // cycle: unsatisfiable
            _ => {}
        }
        state.insert(name.to_string(), 1); // in-progress
        let ds = deps.get(name);
        let mut r = true;
        if let Some(ds) = ds {
            for d in ds {
                if !registered.contains(d.as_str()) {
                    r = false;
                    break;
                }
                if !Self::dfs_ready(d, deps, registered, ready, state) {
                    r = false;
                    break;
                }
            }
        }
        state.insert(name.to_string(), 2); // done
        ready.insert(name.to_string(), r);
        r
    }

    /// Shared handle to the reactive dependency state (for the
    /// `tool_dependencies` introspection tool).
    pub fn dependency_state(&self) -> Arc<RwLock<DependencyState>> {
        self.deps.clone()
    }

    /// Cordis P3: whether the tool registered under `name` is
    /// runtime-revertible (P1). `None` if the tool isn't registered or
    /// doesn't participate in the revertible-effect system.
    pub fn revertible_of(&self, name: &str) -> Option<bool> {
        self.tools.get(name).and_then(|t| t.revertible())
    }

    /// Test helper: a read guard over the live dependency state, bound to
    /// `self`'s lifetime (avoids holding a guard on a dropped Arc).
    #[cfg(test)]
    pub fn read_deps(&self) -> std::sync::RwLockReadGuard<'_, DependencyState> {
        self.deps.read().unwrap()
    }

    #[allow(dead_code)]
    pub fn with_native_tools() -> Self {
        Self::with_tools(None, None, None, None, None, None)
    }

    /// Native tools plus any Lua scripted tools found on disk.
    ///
    /// `prompt_state` is the live harness state shared with the agent
    /// loop; evolution tools update it so harness edits apply without
    /// a restart. Pass `None` (tests) to use an isolated state.
    ///
    /// `goals` is the shared goal state (same `Arc` the agent loop
    /// injects into the system context). Pass `None` to run without
    /// the persistent-goals feature.
    pub fn with_tools(
        scripted_dir: Option<&Path>,
        harness_dir: Option<&Path>,
        prompt_state: Option<std::sync::Arc<std::sync::RwLock<crate::harness::PromptState>>>,
        goals: Option<std::sync::Arc<std::sync::RwLock<Vec<crate::goals::GoalState>>>>,
        local_harness_dir: Option<&Path>,
        hooks_dir: Option<&Path>,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(bash::BashTool::new()));
        registry.register(Arc::new(files::ReadFileTool::new()));
        registry.register(Arc::new(files::WriteFileTool::new()));
        registry.register(Arc::new(git::GitStatusTool::new()));
        registry.register(Arc::new(git::GitDiffTool::new()));
        registry.register(Arc::new(git::GitLogTool::new()));
        // Evolution tool: the model can write new Lua tools at runtime.
        let tools_dir = scripted_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(scripted::default_tools_dir);
        let hot_queue = registry.hot_queue.clone();
        let hot_queue2 = hot_queue.clone();
        let hot_queue3 = hot_queue.clone();
        registry.register(Arc::new(evolve::CreateLuaToolTool {
            tools_dir: tools_dir.clone(),
            register: Box::new(move |name: &str, path: std::path::PathBuf| {
                if let Ok(mut q) = hot_queue.try_lock() {
                    q.push(HotOp::register(name, path));
                }
            }),
            unregister: Box::new(move |name: &str| {
                if let Ok(mut q) = hot_queue2.try_lock() {
                    q.push(HotOp::unregister(name));
                }
            }),
        }));
        // Harness evolution: the model can improve its own instruction
        // layer (system prompt / skills). Follows --harness-dir so the
        // tool edits exactly what the agent loaded.
        let harness_dir = harness_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(harness::default_harness_dir);
        // Cordis P1: runtime revertible-effect tracker, shared between the
        // write side (improve_harness registers an inverse per write) and
        // the revert side (revert_effects rolls them back LIFO at runtime).
        let effects = std::sync::Arc::new(crate::revertible::EffectTracker::new());
        registry.register(Arc::new(evolve::ImproveHarnessTool {
            harness_dir: harness_dir.clone(),
            local_dir: local_harness_dir.map(Path::to_path_buf),
            prompt_state: prompt_state.clone(),
            hooks_dir: hooks_dir.map(Path::to_path_buf),
            effects: Some(effects.clone()),
        }));
        // Runtime rollback of the above: undo the most recent improve_harness
        // write(s) in-process, LIFO. The model gets a one-call "undo my last
        // harness edit" without reaching for git or the on-disk archive.
        registry.register(Arc::new(evolve::RevertEffectsTool { tracker: effects }));
        // Persistent goals: the model declares long-running objectives;
        // active goals are injected into the system context each turn.
        // (The same `Arc` is handed to the agent loop, so tool writes
        // are visible to the loop without a restart.)
        let goals = goals.unwrap_or_else(|| Arc::new(RwLock::new(Vec::new())));
        registry.register(Arc::new(goals_tool::ManageGoalsTool {
            goals_dir: harness_dir.clone(),
            goals: goals.clone(),
        }));
        // Version library: list & roll back model-written assets.
        registry.register(Arc::new(versions::SkillHistoryTool {
            harness_dir: harness_dir.clone(),
            tools_dir: tools_dir.clone(),
            hooks_dir: hooks_dir.map(Path::to_path_buf),
        }));
        registry.register(Arc::new(versions::SkillRollbackTool {
            harness_dir: harness_dir.clone(),
            tools_dir: tools_dir.clone(),
            hooks_dir: hooks_dir.map(Path::to_path_buf),
            register: Some(Box::new(move |name: &str, path: std::path::PathBuf| {
                if let Ok(mut q) = hot_queue3.try_lock() {
                    q.push(HotOp::register(name, path));
                }
            })),
            prompt_state: prompt_state.clone(),
        }));
        registry.register(Arc::new(versions::VersionsGcTool {
            harness_dir,
            tools_dir: tools_dir.clone(),
            hooks_dir: hooks_dir.map(Path::to_path_buf),
        }));
        // Cross-process ACP delegation: spawn an external ACP server.
        registry.register(Arc::new(acp::AcpCallTool {
            command: "sigil".to_string(),
            args: vec!["acp".to_string()],
        }));
        if let Some(dir) = scripted_dir {
            registry.register_scripted(dir);
        }
        // Cordis P2: introspection — let the agent (or a human) inspect the
        // live tool dependency graph + readiness. Holds a clone of the same
        // shared DependencyState the registry rewrites on every
        // register/unregister, so it always sees the current graph without
        // borrowing the registry that holds it (no ownership cycle).
        registry.register(Arc::new(ToolDependenciesTool {
            state: registry.dependency_state(),
        }));
        registry
    }

    /// Register Lua scripted tools found in `dir`.
    pub fn register_scripted(&mut self, dir: &Path) {
        let lua = Arc::new(Mutex::new(LuaRuntime::new().expect("lua runtime init")));
        for path in scripted::discover(dir) {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match scripted::LuaTool::new(stem, &path, lua.clone()) {
                Ok(tool) => {
                    self.register(Arc::new(tool));
                }
                Err(e) => eprintln!("warning: skipping scripted tool {stem}: {e}"),
            }
        }
    }

    /// Hot-register tools queued by evolution tools. Called by the agent
    /// loop after each turn so newly created tools are available to the
    /// model on the very next turn.
    pub fn drain_hot(&mut self) {
        let lua = Arc::new(Mutex::new(LuaRuntime::new().expect("lua runtime init")));
        // try_lock: we're in an async context; the queue is only touched
        // here and by the (sync) evolution tool, so contention is nil.
        let queued: Vec<_> = match self.hot_queue.try_lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        for op in queued {
            let name = op.name.clone();
            if !op.register {
                self.unregister(&name);
                eprintln!("hot-unregistered Lua tool: {name}");
                continue;
            }
            let Some(path) = op.path else { continue };
            match scripted::LuaTool::new(&name, &path, lua.clone()) {
                Ok(tool) => {
                    self.register(Arc::new(tool));
                    eprintln!("hot-registered Lua tool: {name}");
                }
                Err(e) => eprintln!("warning: hot-register {name} failed: {e}"),
            }
        }
    }

    /// Tool specs advertised to the model, in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Execute a tool call by name. Errors are returned as tool output so
    /// the model sees them (never fatal to the loop).
    pub async fn execute(&self, call: &crate::llm::ToolCall) -> anyhow::Result<String> {
        Ok(self.execute_detailed(call).await?.text)
    }

    /// Preserve the tool's actual failure status for protocol consumers.
    pub async fn execute_detailed(
        &self,
        call: &crate::llm::ToolCall,
    ) -> anyhow::Result<ToolExecution> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", call.name))?;
        // Cordis P2 (reactive coeffects): if this tool declared dependencies
        // and any of them is not currently registered, degrade gracefully
        // instead of running against a missing service — the model sees
        // exactly what is missing and can decide (wait, re-register, or
        // drop the call). A tool with no declared dependencies is always
        // ready, so this is a no-op for every pre-existing tool.
        let deps = tool.dependencies();
        let missing: Vec<&str> = deps
            .iter()
            .map(String::as_str)
            .filter(|d| !self.tools.contains_key(*d))
            .collect();
        if !missing.is_empty() {
            return Ok(ToolExecution {
                is_error: true,
                text: format!(
                    "tool '{}' is not ready — its dependencies are not registered yet: {}. Register (or reconnect) the dependency first; this call was not executed.",
                    call.name,
                    missing.join(", ")
                ),
            });
        }
        match tool.run(&call.arguments).await {
            Ok(out) => Ok(ToolExecution {
                text: out.text,
                is_error: false,
            }),
            Err(e) => Ok(ToolExecution {
                text: format!("error: {e}"),
                is_error: true,
            }),
        }
    }
}

/// Cordis P2: introspection tool that lets the agent (or a human) inspect
/// the live tool dependency graph and per-tool readiness. Holds a clone of
/// the registry's shared [`DependencyState`] (the same `Arc<RwLock<..>>`
/// the registry rewrites on every register/unregister), so it always sees
/// the current graph without borrowing the registry that registered it —
/// no ownership cycle.
///
/// Actions: `graph` (every tool that declares dependencies, with its ready
/// state and the specific deps still missing), `blocked` (only the tools
/// that are not ready), `query` (pass `name`: one tool's deps, its
/// readiness, its missing deps, and the reverse edges — which tools depend
/// on it).
pub struct ToolDependenciesTool {
    state: Arc<RwLock<DependencyState>>,
}

#[async_trait]
impl Tool for ToolDependenciesTool {
    fn name(&self) -> &str {
        "tool_dependencies"
    }

    fn description(&self) -> &str {
        "Inspect the live tool dependency graph (Cordis P2: reactive \
         coeffects / dependency injection). Tools can declare what they \
         depend on (e.g. a self-authored Lua tool's `-- requires:` line); \
         the registry tracks which are ready (all deps registered) and \
         which are blocked. Actions: 'graph' (every dependent tool + its \
         readiness + what's missing), 'blocked' (only tools that are not \
         ready), 'query' (pass 'name': one tool's deps, readiness, missing \
         deps, and the reverse edges — which tools depend on it)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["graph", "blocked", "query"],
                    "description": "'graph' = full dependent list, 'blocked' = not-ready tools only, 'query' = one tool."
                },
                "name": {
                    "type": "string",
                    "description": "Required for action='query': the tool to inspect."
                }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("graph");
        let s = self.state.read().unwrap();
        match action {
            "blocked" => {
                let mut blocked: Vec<&String> = s
                    .ready
                    .iter()
                    .filter(|(_, r)| !**r)
                    .map(|(t, _)| t)
                    .collect();
                blocked.sort();
                if blocked.is_empty() {
                    return Ok(ToolOutput {
                        text: "all tools are ready (no declared dependencies are missing)"
                            .to_string(),
                    });
                }
                let mut out = format!(
                    "{} tool(s) currently blocked on missing dependencies:\n",
                    blocked.len()
                );
                for t in blocked {
                    let miss = s.missing.get(t).cloned().unwrap_or_default();
                    out.push_str(&format!("- {t}: missing {}\n", miss.join(", ")));
                }
                Ok(ToolOutput {
                    text: out.trim_end().to_string(),
                })
            }
            "query" => {
                let Some(name) = args.get("name").and_then(Value::as_str) else {
                    anyhow::bail!("tool_dependencies: action='query' requires 'name'");
                };
                let deps = s.deps.get(name).cloned().unwrap_or_default();
                let ready = s.ready.get(name).copied();
                let missing = s.missing.get(name).cloned().unwrap_or_default();
                let dependents = s.dependents_of(name);
                let mut out = String::new();
                out.push_str(&format!("tool '{name}':\n"));
                out.push_str(&format!(
                    "- ready: {}\n",
                    match ready {
                        Some(true) => "yes",
                        Some(false) => "NO",
                        None => "unknown (not a dependent tool / not registered)",
                    }
                ));
                out.push_str(&format!(
                    "- depends on: {}\n",
                    if deps.is_empty() {
                        "(none)".to_string()
                    } else {
                        deps.join(", ")
                    }
                ));
                if !missing.is_empty() {
                    out.push_str(&format!("- missing deps: {}\n", missing.join(", ")));
                }
                out.push_str(&format!(
                    "- depended on by: {}",
                    if dependents.is_empty() {
                        "(none)".to_string()
                    } else {
                        dependents.join(", ")
                    }
                ));
                Ok(ToolOutput { text: out })
            }
            _ => {
                // "graph" (default): list every tool that declares deps.
                let mut names: Vec<&String> = s
                    .deps
                    .keys()
                    .filter(|t| s.deps.get(*t).map(|ds| !ds.is_empty()).unwrap_or(false))
                    .collect();
                names.sort();
                if names.is_empty() {
                    return Ok(ToolOutput {
                        text:
                            "no tools declare dependencies (every tool is standalone/always-ready)"
                                .to_string(),
                    });
                }
                let mut out = format!("{} tool(s) declare dependencies:\n", names.len());
                for t in names {
                    let ds = &s.deps[t];
                    let ready = s.ready.get(t).copied().unwrap_or(false);
                    let miss = s.missing.get(t).cloned().unwrap_or_default();
                    out.push_str(&format!(
                        "- {t} [{}]: needs {}\n",
                        if ready { "ready" } else { "BLOCKED" },
                        ds.join(", ")
                    ));
                    if !miss.is_empty() {
                        out.push_str(&format!("    missing: {}\n", miss.join(", ")));
                    }
                }
                Ok(ToolOutput {
                    text: out.trim_end().to_string(),
                })
            }
        }
    }
}

/// Helper: build a JSON error payload. Used by scripted/MCP tools.
#[allow(dead_code)]
pub fn error_output(msg: impl Into<String>) -> ToolOutput {
    ToolOutput {
        text: json!({ "error": msg.into() }).to_string(),
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::tools::bash::BashTool;

    #[test]
    fn re_register_same_name_does_not_duplicate_order() {
        let mut reg = ToolRegistry::new();
        reg.register(std::sync::Arc::new(BashTool::new()));
        // Re-register (e.g. a scripted tool shadowing a native one).
        reg.register(std::sync::Arc::new(BashTool::new()));
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "bash");
    }

    // ---- Cordis P2: reactive coeffects / dependency injection ----

    /// A stub tool that can be told which other tools it depends on.
    struct DepTool {
        name: String,
        deps: Vec<String>,
    }
    #[async_trait::async_trait]
    impl Tool for DepTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "dep stub"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn run(&self, _args: &Value) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput {
                text: self.name.clone(),
            })
        }
        fn dependencies(&self) -> Vec<String> {
            self.deps.clone()
        }
    }
    fn dep(name: &str, deps: Vec<&str>) -> std::sync::Arc<dyn Tool> {
        std::sync::Arc::new(DepTool {
            name: name.to_string(),
            deps: deps.into_iter().map(|s| s.to_string()).collect(),
        })
    }

    /// A tool blocked on a missing dep is NOT ready, and reports the
    /// missing dep; registering the dep makes it ready again (the reactive
    /// coeffect).
    #[test]
    fn p2_register_unblocks_dependent_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("a", vec![]));
        reg.register(dep("b", vec!["a"]));
        reg.register(dep("c", vec!["a", "missing"]));
        let s = reg.read_deps();
        assert_eq!(s.ready.get("a"), Some(&true));
        assert_eq!(s.ready.get("b"), Some(&true));
        assert_eq!(s.ready.get("c"), Some(&false), "c is blocked on 'missing'");
        assert_eq!(
            s.missing.get("c").cloned().unwrap_or_default(),
            vec!["missing".to_string()]
        );
        // 'a' is directly depended on by both 'b' and 'c' (sorted).
        assert_eq!(s.dependents_of("a"), vec!["b".to_string(), "c".to_string()]);
        drop(s);
        // Register the missing dep -> 'c' becomes ready (reactive).
        reg.register(dep("missing", vec![]));
        let s = reg.read_deps();
        assert_eq!(
            s.ready.get("c"),
            Some(&true),
            "c is ready once its dep registers"
        );
    }

    /// Unregistering a dependency blocks its dependents reactively.
    #[test]
    fn p2_unregister_blocks_dependent_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("a", vec![]));
        reg.register(dep("b", vec!["a"]));
        assert_eq!(
            reg.dependency_state().read().unwrap().ready.get("b"),
            Some(&true)
        );
        reg.unregister("a");
        let s = reg.read_deps();
        assert_eq!(
            s.ready.get("b"),
            Some(&false),
            "b is blocked once 'a' unregisters"
        );
        assert_eq!(
            s.missing.get("b").cloned().unwrap_or_default(),
            vec!["a".to_string()]
        );
    }

    /// Readiness propagates transitively (b depends on a; c depends on b):
    /// missing 'a' blocks both 'b' and 'c'; adding 'a' readies all.
    #[test]
    fn p2_transitive_readiness() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("b", vec!["a"]));
        reg.register(dep("c", vec!["b"]));
        let s = reg.read_deps();
        assert_eq!(s.ready.get("b"), Some(&false));
        assert_eq!(
            s.ready.get("c"),
            Some(&false),
            "c is blocked transitively via b"
        );
        drop(s);
        reg.register(dep("a", vec![]));
        let s = reg.read_deps();
        assert_eq!(s.ready.get("b"), Some(&true));
        assert_eq!(s.ready.get("c"), Some(&true));
    }

    /// A dependency cycle can never be satisfied -> both tools are blocked
    /// (no infinite loop during recompute).
    #[test]
    fn p2_cycle_is_never_ready() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("x", vec!["y"]));
        reg.register(dep("y", vec!["x"]));
        let s = reg.read_deps();
        assert_eq!(s.ready.get("x"), Some(&false));
        assert_eq!(s.ready.get("y"), Some(&false));
    }

    /// The recompute delta reports exactly which tools became ready.
    #[test]
    fn p2_delta_reports_became_ready() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("b", vec!["a"]));
        let before = reg.recompute_readiness();
        assert!(
            before.became_ready.is_empty(),
            "no flip on a pure recompute"
        );
        let d = reg.register(dep("a", vec![]));
        assert_eq!(d.became_ready, vec!["b".to_string()]);
        // Re-registering an already-ready dep produces no new flip.
        let d2 = reg.register(dep("a", vec![]));
        assert!(d2.became_ready.is_empty());
    }

    /// `execute` degrades gracefully for a tool blocked on a missing dep
    /// (the stub's `run` is never reached); a ready tool executes normally.
    #[tokio::test]
    async fn p2_execute_gates_blocked_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("a", vec![]));
        reg.register(dep("b", vec!["a"]));
        reg.register(dep("c", vec!["missing"]));
        let call = |name: &str| crate::llm::ToolCall {
            id: "1".to_string(),
            name: name.to_string(),
            arguments: json!({}),
        };
        let out = reg.execute(&call("b")).await.unwrap();
        assert_eq!(out, "b", "ready tool runs normally");
        let out = reg.execute(&call("c")).await.unwrap();
        assert!(out.contains("not ready"), "blocked tool degrades: {out}");
        assert!(
            out.contains("missing"),
            "message names the missing dep: {out}"
        );
        assert!(!out.eq("c"), "the blocked tool's body did not run");
    }

    /// A Lua tool's `-- requires:` header becomes its declared
    /// dependencies (the real Cordis `inject` landing point).
    #[test]
    fn p2_lua_requires_header_parses() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-p2-lua-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("needs_tool.lua");
        std::fs::write(
            &path,
            "-- desc: needs two tools\n-- requires: other_tool, another_tool\nreturn 0\n",
        )
        .unwrap();
        let lua = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::lua::LuaRuntime::new().expect("lua init"),
        ));
        let tool = scripted::LuaTool::new("needs_tool", &path, lua).unwrap();
        let mut deps = tool.dependencies();
        deps.sort();
        assert_eq!(
            deps,
            vec!["another_tool".to_string(), "other_tool".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The introspection tool reads the live shared dependency state.
    #[tokio::test]
    async fn p2_tool_dependencies_introspection() {
        let mut reg = ToolRegistry::new();
        reg.register(dep("a", vec![]));
        reg.register(dep("b", vec!["a"]));
        reg.register(dep("c", vec!["missing"]));
        let tdeps = ToolDependenciesTool {
            state: reg.dependency_state(),
        };
        let graph = tdeps.run(&json!({ "action": "graph" })).await.unwrap().text;
        assert!(
            graph.contains("b [ready]: needs a"),
            "graph shows b: {graph}"
        );
        assert!(
            graph.contains("c [BLOCKED]: needs missing"),
            "graph shows c: {graph}"
        );
        assert!(
            graph.contains("missing: missing"),
            "graph names c's missing dep"
        );
        let blocked = tdeps
            .run(&json!({ "action": "blocked" }))
            .await
            .unwrap()
            .text;
        assert!(blocked.contains("c") && !blocked.contains("b ["));
        let query = tdeps
            .run(&json!({ "action": "query", "name": "a" }))
            .await
            .unwrap()
            .text;
        assert!(query.contains("depended on by: b"), "reverse edge: {query}");
    }

    // ---- Cordis P3: revertible annotation (grounded in the P1 wiring) ----

    /// `revertible_of` reflects the actual P1 wiring: `improve_harness`
    /// wired with a live EffectTracker is runtime-revertible (Some(true));
    /// the same tool with `effects: None` is Some(false); a tool that never
    /// opted into the P1 system (default `revertible()`) is None.
    #[test]
    fn p3_revertible_of_reflects_p1_wiring() {
        use crate::tools::evolve::ImproveHarnessTool;
        let dir = std::env::temp_dir().join(format!(
            "sigil-p3-revertible-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let harness = dir.join("harness");
        std::fs::create_dir_all(&harness).unwrap();

        let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
        let mut reg = ToolRegistry::new();
        // Revertible: ImproveHarnessTool with a live EffectTracker.
        reg.register(std::sync::Arc::new(ImproveHarnessTool {
            harness_dir: harness.clone(),
            local_dir: None,
            prompt_state: None,
            hooks_dir: None,
            effects: Some(tracker),
        }));
        // A plain tool that never opts into P1 → None.
        reg.register(dep("plain", vec![]));

        assert_eq!(
            reg.revertible_of("improve_harness"),
            Some(true),
            "wired → Some(true)"
        );
        assert_eq!(reg.revertible_of("plain"), None, "not in P1 → None");
        assert_eq!(
            reg.revertible_of("no_such_tool"),
            None,
            "unregistered → None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
