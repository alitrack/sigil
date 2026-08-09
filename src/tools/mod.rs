//! Tool registry: native tools + scripted LuaJIT tools + evolution tools
//! behind one lookup, so the agent loop and the model treat all identically.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::llm::ToolSpec;
use crate::lua::LuaRuntime;

pub mod bash;
pub mod evolve;
pub mod files;
pub mod git;
pub mod mcp;
pub mod memory;
pub mod scripted;

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
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    /// Tools hot-registered mid-run by evolution tools, drained by the
    /// agent loop after each turn.
    hot_queue: Arc<Mutex<Vec<(String, std::path::PathBuf)>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name.clone(), tool);
        self.order.push(name);
    }

    #[allow(dead_code)]
    pub fn with_native_tools() -> Self {
        Self::with_tools(None)
    }

    /// Native tools plus any Lua scripted tools found on disk.
    pub fn with_tools(scripted_dir: Option<&Path>) -> Self {
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
        registry.register(Arc::new(evolve::CreateLuaToolTool {
            tools_dir: tools_dir.clone(),
            register: Box::new(move |name: &str, path: std::path::PathBuf| {
                if let Ok(mut q) = hot_queue.try_lock() {
                    q.push((name.to_string(), path));
                }
            }),
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
        for (name, path) in queued {
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
