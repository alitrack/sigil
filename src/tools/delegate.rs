//! Sub-agent delegation: the `delegate` tool lets the main agent spin
//! off an independent task. The sub-agent runs its own agent loop
//! (own conversation, shallow copy of the tool registry) and returns
//! its final answer. Isolation: sub-agent turns never enter the main
//! conversation — only the final text does.
//!
//! Since the harness-evolution phase, delegation is **asynchronous
//! registry-style** (mirrors prime-agent's `rlm(...)` handle semantics):
//! `action=spawn` starts the sub-agent and returns a handle immediately;
//! `action=status` / `action=result` poll it. The main loop keeps
//! working while sub-agents run in the background.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{AgentConfig, run_turn};
use crate::llm::provider::LlmProvider;
use crate::tools::{Tool, ToolOutput, ToolRegistry};

/// Lifecycle of one delegated sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateStatus {
    Running,
    Completed,
    Failed,
}

impl DelegateStatus {
    fn as_str(&self) -> &'static str {
        match self {
            DelegateStatus::Running => "running",
            DelegateStatus::Completed => "completed",
            DelegateStatus::Failed => "failed",
        }
    }
}

/// Snapshot of one delegated sub-agent.
#[derive(Debug, Clone)]
pub struct DelegateState {
    pub id: String,
    pub name: String,
    pub status: DelegateStatus,
    pub started_ts: String,
    pub finished_ts: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Shared registry of sub-agent handles, so `delegate` calls across
/// turns (and even across sessions sharing the tool registry) can
/// poll the same sub-agents.
#[derive(Default)]
pub struct DelegateRegistry {
    states: tokio::sync::Mutex<HashMap<String, DelegateState>>,
    next: AtomicU64,
}

impl DelegateRegistry {
    async fn insert(&self, state: DelegateState) {
        self.states.lock().await.insert(state.id.clone(), state);
    }

    async fn update<F: FnOnce(&mut DelegateState)>(&self, id: &str, f: F) {
        if let Some(st) = self.states.lock().await.get_mut(id) {
            f(st);
        }
    }

    async fn snapshot(&self) -> Vec<DelegateState> {
        self.states.lock().await.values().cloned().collect()
    }
}

/// Tool that delegates a sub-task to a fresh agent turn.
pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    registry: ToolRegistry,
    config: AgentConfig,
    delegates: Arc<DelegateRegistry>,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            registry,
            config,
            delegates: Arc::new(DelegateRegistry::default()),
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained sub-task to a fresh sub-agent. \
         Actions: 'spawn' (default — start a sub-agent and return its \
         handle immediately; it keeps running in the background while \
         you work), 'status' (id — running/completed/failed), 'result' \
         (id — the sub-agent's final summary once completed), 'list' \
         (all sub-agents). Use for parallel exploration, research, or \
         long independent subtasks; the result is a text summary, not \
         the full transcript. Poll with action=status then collect \
         with action=result."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "status", "result", "list"],
                    "description": "spawn (default): start a sub-agent; status: poll one; result: collect one's summary; list: enumerate all"
                },
                "task": {
                    "type": "string",
                    "description": "For spawn: the self-contained task for the sub-agent"
                },
                "id": {
                    "type": "string",
                    "description": "Sub-agent handle id (from spawn / list) — required for status/result"
                }
            },
            "required": []
        })
    }

    async fn run(&self, args: &Value) -> Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("spawn");

        match action {
            "spawn" => {
                let task = args
                    .get("task")
                    .and_then(Value::as_str)
                    .context("delegate spawn requires a 'task' string")?;
                let name = task.chars().take(40).collect::<String>();
                let id = format!("d{:05}", self.delegates.next.fetch_add(1, Ordering::SeqCst));
                let ts = crate::session::chrono_like_timestamp();
                self.delegates
                    .insert(DelegateState {
                        id: id.clone(),
                        name,
                        status: DelegateStatus::Running,
                        started_ts: ts.clone(),
                        finished_ts: None,
                        result: None,
                        error: None,
                    })
                    .await;

                let provider = self.provider.clone();
                let mut sub_tools = self.registry.clone_shallow();
                let config = self.config.clone();
                let task = task.to_string();
                let delegates = self.delegates.clone();
                let spawn_id = id.clone();
                tokio::spawn(async move {
                    // Sub-agents run their own loop, no approver
                    // (non-interactive: the gate's default rejection
                    // stands). The result lands in the registry.
                    let outcome = run_turn(provider.as_ref(), &mut sub_tools, &config, &task).await;
                    match outcome {
                        Ok(answer) => {
                            delegates
                                .update(&spawn_id, |st| {
                                    st.status = DelegateStatus::Completed;
                                    st.finished_ts = Some(crate::session::chrono_like_timestamp());
                                    st.result = Some(answer);
                                })
                                .await;
                        }
                        Err(e) => {
                            delegates
                                .update(&spawn_id, |st| {
                                    st.status = DelegateStatus::Failed;
                                    st.finished_ts = Some(crate::session::chrono_like_timestamp());
                                    st.error = Some(e.to_string());
                                })
                                .await;
                        }
                    }
                });

                Ok(ToolOutput {
                    text: format!(
                        "delegate '{id}' spawned (status: running). Keep working; poll with action=status, collect with action=result."
                    ),
                })
            }
            "status" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .context("delegate status requires 'id'")?;
                let states = self.delegates.snapshot().await;
                let st = states
                    .iter()
                    .find(|s| s.id == id)
                    .context("no such delegate id (see action=list)")?;
                Ok(ToolOutput {
                    text: match st.status {
                        DelegateStatus::Running => {
                            format!("delegate '{id}' is running (spawned {})", st.started_ts)
                        }
                        DelegateStatus::Completed => format!(
                            "delegate '{id}' completed (finished {}). Collect with action=result.",
                            st.finished_ts.as_deref().unwrap_or("?")
                        ),
                        DelegateStatus::Failed => format!(
                            "delegate '{id}' failed: {}",
                            st.error.as_deref().unwrap_or("unknown error")
                        ),
                    },
                })
            }
            "result" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .context("delegate result requires 'id'")?;
                let states = self.delegates.snapshot().await;
                let st = states
                    .iter()
                    .find(|s| s.id == id)
                    .context("no such delegate id (see action=list)")?;
                match (&st.status, &st.result) {
                    (DelegateStatus::Completed, Some(r)) => Ok(ToolOutput {
                        text: format!("delegate '{id}' result:\n{r}"),
                    }),
                    (DelegateStatus::Failed, _) => Ok(ToolOutput {
                        text: format!(
                            "delegate '{id}' failed: {}",
                            st.error.as_deref().unwrap_or("unknown error")
                        ),
                    }),
                    _ => Ok(ToolOutput {
                        text: format!(
                            "delegate '{id}' is still running (spawned {}). Poll action=status, then action=result once completed.",
                            st.started_ts
                        ),
                    }),
                }
            }
            "list" => {
                let states = self.delegates.snapshot().await;
                if states.is_empty() {
                    return Ok(ToolOutput {
                        text: "no sub-agents running or finished".to_string(),
                    });
                }
                let mut text = String::from("sub-agents:");
                for st in states.iter() {
                    text.push_str(&format!(
                        "\n- {} [{}] — {}",
                        st.id,
                        st.status.as_str(),
                        st.name
                    ));
                }
                Ok(ToolOutput { text })
            }
            _ => anyhow::bail!("unknown action '{action}' (spawn|status|result|list)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_schema_is_valid() {
        let tool = DelegateTool {
            provider: Arc::new(
                crate::llm::openai::OpenAiProvider::with_fallbacks(
                    crate::llm::openai::OpenAiConfig {
                        base_url: "http://localhost:1/v1".into(),
                        api_key: "x".into(),
                    },
                    vec![],
                )
                .unwrap(),
            ),
            registry: ToolRegistry::with_tools(None, None, None, None),
            config: AgentConfig {
                model: "test".into(),
                max_steps: 3,
                prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                    crate::harness::PromptState {
                        system_prompt: "sys".into(),
                        skills: vec![],
                        fingerprint: None,
                    },
                )),
                max_inline_skills: 0,
                sync_dir: None,
                max_context_messages: 0,
                max_context_tokens: 0,
                goals: None,
            },
            delegates: Arc::new(DelegateRegistry::default()),
        };
        assert_eq!(tool.name(), "delegate");
        let p = tool.parameters();
        let actions = p["properties"]["action"]["enum"].as_array().unwrap();
        let names: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["spawn", "status", "result", "list"]);
    }

    #[tokio::test]
    async fn registry_tracks_lifecycle() {
        let reg = DelegateRegistry::default();
        reg.insert(DelegateState {
            id: "d00000".into(),
            name: "first".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
        })
        .await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].status, DelegateStatus::Running);

        reg.update("d00000", |st| {
            st.status = DelegateStatus::Completed;
            st.result = Some("done".into());
        })
        .await;
        let snap = reg.snapshot().await;
        assert_eq!(snap[0].status, DelegateStatus::Completed);
        assert_eq!(snap[0].result.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn registry_survives_concurrent_subagent_updates() {
        // Many sub-agents finishing at once (each takes the registry
        // lock briefly) must all land their results — no silent drop.
        let reg = Arc::new(DelegateRegistry::default());
        let mut tasks = Vec::new();
        for i in 0..8 {
            let reg = reg.clone();
            tasks.push(tokio::spawn(async move {
                let id = format!("d{i:05}");
                reg.insert(DelegateState {
                    id: id.clone(),
                    name: format!("task-{i}"),
                    status: DelegateStatus::Running,
                    started_ts: "t".into(),
                    finished_ts: None,
                    result: None,
                    error: None,
                })
                .await;
                // Simulate the sub-agent finishing right away.
                reg.update(&id, |st| {
                    st.status = DelegateStatus::Completed;
                    st.result = Some(format!("result-{i}"));
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 8, "every sub-agent must land in the registry");
        let mut ids: Vec<&str> = snap.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "d00000", "d00001", "d00002", "d00003", "d00004", "d00005", "d00006", "d00007"
            ]
        );
        assert!(snap.iter().all(|s| s.status == DelegateStatus::Completed));
        assert!(snap.iter().all(|s| s.result.is_some()));
    }
}
