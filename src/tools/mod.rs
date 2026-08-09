//! Tool registry: native tools + scripted LuaJIT tools + evolution tools
//! behind one lookup, so the agent loop and the model treat all identically.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::harness;
use crate::llm::ToolSpec;
use crate::lua::LuaRuntime;

pub mod acp;
pub mod bash;
pub mod delegate;
pub mod evolve;
pub mod files;
pub mod git;
pub mod mcp;
pub mod memory;
pub mod scripted;
pub mod versions;

#[cfg(test)]
mod scripted_tests;

#[cfg(test)]
mod evolve_tests;

/// What a tool returns to the agent loop.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
}

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput>;
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

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        // Re-registering the same name replaces the impl but must not
        // duplicate the order entry (a scripted tool may shadow a
        // native tool of the same name — e.g. a model-authored
        // mcp_resources.lua — without breaking the advertised list).
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Remove a tool by name (idempotent). Used by evolution tools to
    /// delete scripted tools at runtime.
    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
        self.order.retain(|n| n != name);
    }

    #[allow(dead_code)]
    pub fn with_native_tools() -> Self {
        Self::with_tools(None, None)
    }

    /// Native tools plus any Lua scripted tools found on disk.
    pub fn with_tools(scripted_dir: Option<&Path>, harness_dir: Option<&Path>) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(bash::BashTool));
        registry.register(Arc::new(files::ReadFileTool));
        registry.register(Arc::new(files::WriteFileTool));
        registry.register(Arc::new(git::GitStatusTool));
        registry.register(Arc::new(git::GitDiffTool));
        registry.register(Arc::new(git::GitLogTool));
        // Evolution tool: the model can write new Lua tools at runtime.
        let tools_dir = scripted_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(scripted::default_tools_dir);
        let hot_queue = registry.hot_queue.clone();
        let hot_queue2 = hot_queue.clone();
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
        registry.register(Arc::new(evolve::ImproveHarnessTool {
            harness_dir: harness_dir.clone(),
        }));
        // Version library: list & roll back model-written assets.
        registry.register(Arc::new(versions::SkillHistoryTool {
            harness_dir: harness_dir.clone(),
            tools_dir: tools_dir.clone(),
        }));
        registry.register(Arc::new(versions::SkillRollbackTool {
            harness_dir,
            tools_dir,
        }));
        // Cross-process ACP delegation: spawn an external ACP server.
        registry.register(Arc::new(acp::AcpCallTool {
            command: "sigil".to_string(),
            args: vec!["acp".to_string()],
        }));
        if let Some(dir) = scripted_dir {
            registry.register_scripted(dir);
        }
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
                Ok(tool) => self.register(Arc::new(tool)),
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
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", call.name))?;
        match tool.run(&call.arguments).await {
            Ok(out) => Ok(out.text),
            Err(e) => Ok(format!("error: {e}")),
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
        reg.register(std::sync::Arc::new(BashTool));
        // Re-register (e.g. a scripted tool shadowing a native one).
        reg.register(std::sync::Arc::new(BashTool));
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "bash");
    }
}
