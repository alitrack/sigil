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

use crate::agent::{AgentConfig, run_messages};
use crate::llm::provider::{ChatMessage, LlmProvider};
use crate::tools::subagent;
use crate::tools::{Tool, ToolOutput, ToolRegistry};

/// Lifecycle of one delegated sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DelegateState {
    pub id: String,
    pub name: String,
    pub status: DelegateStatus,
    pub started_ts: String,
    pub finished_ts: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    /// The original task text (kept so a resumed sub-agent can be
    /// rebuilt with the full task context).
    pub task: String,
    /// Handle id this sub-agent was resumed from, if any (lineage).
    pub resume_from: Option<String>,
    /// Number of agent steps the sub-agent took, once finished.
    /// (Absent for legacy states written before this field existed.)
    #[serde(default)]
    pub steps: Option<usize>,
}

impl DelegateState {
    /// Serialize this state to a `meta.json` under `dir/<id>/`
    /// (atomic: write a tmp file, then rename).
    fn persist(&self, dir: &std::path::Path) -> std::io::Result<()> {
        let sub = dir.join(&self.id);
        std::fs::create_dir_all(&sub)?;
        let path = sub.join("meta.json");
        let tmp = sub.join("meta.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// Rebuild a state from `dir/<id>/meta.json`.
    fn load(dir: &std::path::Path, id: &str) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(dir.join(id).join("meta.json"))?;
        Ok(serde_json::from_str(&raw)?)
    }
}

/// Default delegates dir: `~/.sigil/delegates/`.
pub fn default_delegates_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".sigil")
        .join("delegates")
}

/// Shared registry of sub-agent handles, so `delegate` calls across
/// turns (and even across sessions sharing the tool registry) can
/// poll the same sub-agents. When a `dir` is configured every state
/// is also persisted as `<dir>/<id>/meta.json` and the registry is
/// recovered from disk on startup — sub-agents survive process
/// restarts and can be resumed.
#[derive(Default)]
pub struct DelegateRegistry {
    states: tokio::sync::Mutex<HashMap<String, DelegateState>>,
    next: AtomicU64,
    dir: Option<std::path::PathBuf>,
}

impl DelegateRegistry {
    /// Build a registry. When `dir` is set, `<dir>/*/meta.json` are
    /// scanned synchronously and every known sub-agent is loaded back
    /// in — delegates survive process restarts (and the next handle id
    /// is re-indexed above the recovered ones).
    fn new(dir: Option<std::path::PathBuf>) -> Self {
        let (states, next) = if let Some(dir) = &dir {
            let mut m = HashMap::new();
            let mut max = 0u64;
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if let Ok(st) = DelegateState::load(dir, id) {
                        if let Some(n) = st.id.strip_prefix('d').and_then(|s| s.parse::<u64>().ok())
                        {
                            max = max.max(n);
                        }
                        m.insert(st.id.clone(), st);
                    }
                }
            }
            (m, max + 1)
        } else {
            (HashMap::new(), 0)
        };
        Self {
            states: tokio::sync::Mutex::new(states),
            next: AtomicU64::new(next),
            dir,
        }
    }

    async fn insert(&self, state: DelegateState) {
        if let Some(dir) = &self.dir {
            let _ = state.persist(dir);
        }
        self.states.lock().await.insert(state.id.clone(), state);
    }

    async fn update<F: FnOnce(&mut DelegateState)>(&self, id: &str, f: F) {
        let mut guard = self.states.lock().await;
        let Some(st) = guard.get_mut(id) else {
            return;
        };
        f(st);
        if let Some(dir) = &self.dir {
            let _ = st.persist(dir);
        }
    }

    async fn snapshot(&self) -> Vec<DelegateState> {
        self.states.lock().await.values().cloned().collect()
    }

    /// Remove a sub-agent's record (and its `<dir>/<id>/` directory on
    /// disk, keeping the two in sync). Running sub-agents are refused:
    /// their background task would still be updating the entry.
    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        let mut guard = self.states.lock().await;
        let Some(st) = guard.get(id) else {
            anyhow::bail!("no such delegate id '{id}'");
        };
        if st.status == DelegateStatus::Running {
            anyhow::bail!(
                "delegate '{id}' is still running — wait for it to finish before deleting"
            );
        }
        guard.remove(id);
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir.join(id));
        }
        Ok(())
    }
}

/// Tool that delegates a sub-task to a fresh agent turn.
pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    registry: ToolRegistry,
    config: AgentConfig,
    delegates: Arc<DelegateRegistry>,
    agents: Arc<subagent::AgentRegistry>,
    description: std::sync::OnceLock<String>,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: AgentConfig,
        delegates_dir: Option<std::path::PathBuf>,
        harness_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            provider,
            registry,
            config,
            delegates: Arc::new(DelegateRegistry::new(delegates_dir)),
            agents: Arc::new(subagent::AgentRegistry::new(harness_dir.as_deref())),
            description: std::sync::OnceLock::new(),
        }
    }
}

impl DelegateTool {
    /// Apply a sub-agent type definition to a sub-agent's config and
    /// tool registry: append the type's system prompt to a *deep copy*
    /// of the parent's prompt state (the shared Arc must never be
    /// mutated) and drop every tool outside the type's allowlist.
    /// Testable on its own, without spawning an LLM sub-agent.
    fn apply_subagent_type(
        config: &AgentConfig,
        sub_tools: &mut ToolRegistry,
        def: &subagent::AgentDefinition,
    ) -> AgentConfig {
        let mut config = config.clone();
        let mut ps = config
            .prompt_state
            .read()
            .expect("prompt_state poisoned")
            .clone();
        ps.system_prompt = format!("{}\n\n{}", ps.system_prompt, def.prompt);
        config.prompt_state = std::sync::Arc::new(std::sync::RwLock::new(ps));
        if let Some(tools) = &def.tools {
            let names: Vec<String> = sub_tools.specs().iter().map(|s| s.name.clone()).collect();
            for n in names {
                if !tools.contains(&n) {
                    sub_tools.unregister(&n);
                }
            }
        }
        config
    }

    /// Start a sub-agent for `task` and return its handle. Used by
    /// both `spawn` and `resume` (which passes the previous run's
    /// outcome as context and records the lineage). An optional
    /// `subagent_type` applies the type's system-prompt suffix and
    /// tool allowlist (the OpenHands AgentDefinition pattern); the
    /// parent's system prompt is never mutated (deep-copied first).
    async fn spawn_subagent(
        &self,
        task: String,
        resume_from: Option<String>,
        subagent_type: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut sub_tools = self.registry.clone_shallow();
        let config = if let Some(ty) = subagent_type {
            let def = self.agents.resolve(ty).ok_or_else(|| {
                let avail = self
                    .agents
                    .list()
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::anyhow!(
                    "no such sub-agent type '{ty}' (available: {avail}; \
                     omit subagent_type for the default untyped sub-agent)"
                )
            })?;
            Self::apply_subagent_type(&self.config, &mut sub_tools, def)
        } else {
            self.config.clone()
        };
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
                task: task.clone(),
                resume_from,
                steps: None,
            })
            .await;

        let provider = self.provider.clone();
        // Report template (borrowed from smolagents' managed
        // agents): the sub-agent is asked to structure its
        // final report so the parent can see both the answer
        // and the key steps that produced it.
        let task = format!(
            "{task}\n\n\
             When you finish, end your reply with a short report:\n\
             <final_answer>\n  (the answer to the task)\n</final_answer>\n\
             <key_steps>\n  - 2-5 bullet points of what you did\n</key_steps>\n"
        );
        let delegates = self.delegates.clone();
        let spawn_id = id.clone();
        tokio::spawn(async move {
            // Sub-agents run their own loop, no approver
            // (non-interactive: the gate's default rejection
            // stands). The result lands in the registry.
            // Use run_messages (not run_turn) so the run metadata —
            // the step count — is recorded too.
            let messages = vec![
                ChatMessage::system(config.system_prompt_for(&task)),
                ChatMessage::user(task.clone()),
            ];
            let outcome = run_messages(
                provider.as_ref(),
                &mut sub_tools,
                &config,
                &messages,
                None,
                None,
            )
            .await;
            match outcome {
                Ok(res) => {
                    delegates
                        .update(&spawn_id, |st| {
                            st.status = DelegateStatus::Completed;
                            st.finished_ts = Some(crate::session::chrono_like_timestamp());
                            st.result = Some(res.output);
                            st.steps = Some(res.steps as usize);
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
        Ok(id)
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &str {
        // Dynamic listing of registered sub-agent types (the OpenHands
        // get_factory_info pattern, simplified): the model sees which
        // typed sub-agents exist and what tools each one gets. Built
        // lazily once per tool instance.
        self.description.get_or_init(|| {
            let mut d = String::from(
                "Delegate a self-contained sub-task to a fresh sub-agent. \
                 Actions: 'spawn' (default — start a sub-agent and return its \
                 handle immediately; it keeps running in the background while \
                 you work), 'status' (id — running/completed/failed), 'result' \
                 (id — the sub-agent's final summary once completed), 'resume' \
                 (id + task — continue a finished/failed sub-agent with a new \
                 instruction: its original task and final report are fed back \
                 as context, and a fresh handle is returned), 'list' \
                 (all sub-agents), 'delete' (id — remove a finished \
                 sub-agent's record and free its handle). Use for parallel \
                 exploration, research, or \
                 long independent subtasks; the result is a text summary, not \
                 the full transcript. Poll with action=status then collect \
                 with action=result.",
            );
            let types = self.agents.list();
            if types.is_empty() {
                d.push_str(
                    " No sub-agent types are defined (create \
                            <harness-dir>/agents/<name>.md or ~/.sigil/agents/<name>.md).",
                );
            } else {
                d.push_str(
                    " Available sub-agent types (pass one via \
                            subagent_type; omit for the default untyped sub-agent):",
                );
                for t in types {
                    d.push_str(&format!("\n- {} — {}", t.name, t.description));
                    if let Some(tools) = &t.tools {
                        d.push_str(&format!(" [tools: {}]", tools.join(", ")));
                    }
                }
            }
            d
        })
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "status", "result", "resume", "list", "delete"],
                    "description": "spawn (default): start a sub-agent; status: poll one; result: collect one's summary; resume: continue a finished/failed sub-agent with a new instruction (returns a fresh handle); list: enumerate all; delete: remove a finished sub-agent's record"
                },
                "task": {
                    "type": "string",
                    "description": "the task for the sub-agent (required for spawn and resume)"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "optional: the sub-agent type to use (see the description for the available types; each type brings its own system-prompt suffix and tool allowlist)"
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
                    .context("delegate spawn requires a 'task' string")?
                    .to_string();
                let id = self
                    .spawn_subagent(
                        task,
                        None,
                        args.get("subagent_type").and_then(Value::as_str),
                    )
                    .await?;
                Ok(ToolOutput {
                    text: format!(
                        "delegate '{id}' spawned (status: running). Keep working; poll with action=status, collect with action=result."
                    ),
                })
            }
            "resume" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .context("delegate resume requires 'id'")?;
                let new_task = args
                    .get("task")
                    .and_then(Value::as_str)
                    .context("delegate resume requires a 'task' string")?;
                let states = self.delegates.snapshot().await;
                let st = states
                    .iter()
                    .find(|s| s.id == id)
                    .context("no such delegate id (see action=list)")?;
                if st.status == DelegateStatus::Running {
                    anyhow::bail!(
                        "delegate '{id}' is still running; wait for it to finish before resuming"
                    );
                }
                // Rebuild the task with the earlier run's outcome as
                // context (the OpenHands partial-result pattern: a
                // non-finished/stopped sub-agent's partial result goes
                // back to the parent, which can then continue it).
                let prev = match (&st.result, &st.error) {
                    (Some(r), _) => {
                        format!("Previous progress (the earlier run's final report):\n{r}")
                    }
                    (None, Some(e)) => format!("The earlier run failed: {e}"),
                    (None, None) => "The earlier run ended without a recorded result.".to_string(),
                };
                let task = format!(
                    "{}\n\nCONTEXT FROM EARLIER RUN (delegate {}):\n{}\n\nNew instruction: {new_task}",
                    st.task, st.id, prev
                );
                let new_id = self
                    .spawn_subagent(
                        task,
                        Some(st.id.clone()),
                        args.get("subagent_type").and_then(Value::as_str),
                    )
                    .await?;
                Ok(ToolOutput {
                    text: format!(
                        "delegate '{new_id}' spawned as a continuation of '{id}' (status: running). Poll with action=status, collect with action=result."
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
                        text: format!(
                            "delegate '{id}' report (parse the <final_answer> / <key_steps> tags):\n{r}"
                        ),
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
                    let steps = st
                        .steps
                        .map(|n| format!(" ({n} steps)"))
                        .unwrap_or_default();
                    text.push_str(&format!(
                        "\n- {} [{}] — {}{}{}",
                        st.id,
                        st.status.as_str(),
                        st.name,
                        st.resume_from
                            .as_deref()
                            .map(|r| format!(" (resumed from {r})"))
                            .unwrap_or_default(),
                        steps
                    ));
                }
                Ok(ToolOutput { text })
            }
            "delete" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("delete requires 'id'"))?;
                self.delegates.remove(id).await?;
                Ok(ToolOutput {
                    text: format!("delegate '{id}' deleted"),
                })
            }
            _ => anyhow::bail!("unknown action '{action}' (spawn|status|result|list|delete)"),
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
            registry: ToolRegistry::with_tools(None, None, None, None, None),
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
                goals_dir: None,
                planning_interval: 0,
            },
            delegates: Arc::new(DelegateRegistry::default()),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
        };
        assert_eq!(tool.name(), "delegate");
        let p = tool.parameters();
        let actions = p["properties"]["action"]["enum"].as_array().unwrap();
        let names: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            names,
            vec!["spawn", "status", "result", "resume", "list", "delete"]
        );
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
            task: "first task".into(),
            resume_from: None,
            steps: None,
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
                    task: format!("task-{i}"),
                    resume_from: None,
                    steps: None,
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

    #[tokio::test]
    async fn registry_persists_meta_to_disk() {
        let dir =
            std::env::temp_dir().join(format!("sigil-delegate-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = DelegateRegistry::new(Some(dir.clone()));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "persist me".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "some task".into(),
            resume_from: None,
            steps: None,
        })
        .await;
        reg.update("d00001", |st| {
            st.status = DelegateStatus::Completed;
            st.result = Some("done".into());
        })
        .await;

        let meta_path = dir.join("d00001").join("meta.json");
        assert!(meta_path.exists(), "meta.json must be written to disk");
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        let back: DelegateState = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.id, "d00001");
        assert_eq!(back.status, DelegateStatus::Completed);
        assert_eq!(back.result.as_deref(), Some("done"));
        assert_eq!(back.task, "some task");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn steps_survive_persistence_and_legacy_files_default() {
        // Steps recorded on completion are written to meta.json...
        let dir = std::env::temp_dir().join(format!("sigil-delegate-steps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = DelegateRegistry::new(Some(dir.clone()));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "steppy".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
        })
        .await;
        reg.update("d00001", |st| {
            st.status = DelegateStatus::Completed;
            st.result = Some("done".into());
            st.steps = Some(7);
        })
        .await;
        let back: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00001").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back.steps, Some(7), "steps persisted to meta.json");

        // ...and a legacy meta.json without the field still loads
        // (serde default → None), so old delegate dirs keep working.
        std::fs::write(
            dir.join("d00001").join("meta.json"),
            serde_json::to_string(&json!({
                "id": "d00001",
                "name": "legacy",
                "status": "Completed",
                "started_ts": "t0",
                "finished_ts": null,
                "result": "old",
                "error": null,
                "task": "task",
                "resume_from": null
            }))
            .unwrap(),
        )
        .unwrap();
        let legacy: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00001").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            legacy.steps, None,
            "legacy meta.json without steps loads fine"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_shows_step_counts_when_present() {
        let reg = Arc::new(DelegateRegistry::default());
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "with steps".into(),
            status: DelegateStatus::Completed,
            started_ts: "t0".into(),
            finished_ts: Some("t1".into()),
            result: Some("done".into()),
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: Some(7),
        })
        .await;
        reg.insert(DelegateState {
            id: "d00002".into(),
            name: "no steps".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
        })
        .await;
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
            registry: ToolRegistry::with_tools(None, None, None, None, None),
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
                goals_dir: None,
                planning_interval: 0,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
        };
        let out = tool.run(&json!({"action": "list"})).await.unwrap();
        assert!(
            out.text.contains("(7 steps)"),
            "completed sub-agent shows step count: {}",
            out.text
        );
        assert!(
            !out.text.contains("no steps") || !out.text.contains("(0 steps)"),
            "running sub-agent without steps shows no count"
        );
        assert!(!out.text.contains("(0 steps)"), "no '0 steps' shown");
    }

    #[tokio::test]
    async fn delete_removes_record_and_disk_dir() {
        let dir = std::env::temp_dir().join(format!("sigil-delegate-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = Arc::new(DelegateRegistry::new(Some(dir.clone())));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "to delete".into(),
            status: DelegateStatus::Completed,
            started_ts: "t0".into(),
            finished_ts: Some("t1".into()),
            result: Some("done".into()),
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: Some(3),
        })
        .await;
        assert!(dir.join("d00001").join("meta.json").exists());

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
            registry: ToolRegistry::with_tools(None, None, None, None, None),
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
                goals_dir: None,
                planning_interval: 0,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
        };

        let out = tool
            .run(&json!({"action": "delete", "id": "d00001"}))
            .await
            .unwrap();
        assert_eq!(out.text, "delegate 'd00001' deleted");
        assert!(
            reg.snapshot().await.is_empty(),
            "record removed from registry"
        );
        assert!(
            !dir.join("d00001").exists(),
            "meta.json dir removed from disk"
        );

        // Unknown id → clear error.
        let err = tool
            .run(&json!({"action": "delete", "id": "d99999"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such delegate id"), "err: {err}");

        // Running sub-agent → refused (background task still writing).
        reg.insert(DelegateState {
            id: "d00002".into(),
            name: "still going".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
        })
        .await;
        let err = tool
            .run(&json!({"action": "delete", "id": "d00002"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("still running"),
            "running delegate refused: {err}"
        );
        assert!(
            reg.snapshot().await.iter().any(|st| st.id == "d00002"),
            "running delegate survives delete"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_recovers_from_disk_and_reindexes_next() {
        let dir =
            std::env::temp_dir().join(format!("sigil-delegate-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let reg = DelegateRegistry::new(Some(dir.clone()));
            reg.insert(DelegateState {
                id: "d00042".into(),
                name: "old run".into(),
                status: DelegateStatus::Completed,
                started_ts: "t0".into(),
                finished_ts: Some("t1".into()),
                result: Some("old result".into()),
                error: None,
                task: "old task".into(),
                resume_from: None,
                steps: None,
            })
            .await;
        }
        // A fresh registry (new process) recovers the state from disk.
        let reg2 = DelegateRegistry::new(Some(dir.clone()));
        let snap = reg2.snapshot().await;
        assert_eq!(snap.len(), 1, "recovered from disk");
        assert_eq!(snap[0].id, "d00042");
        assert_eq!(snap[0].result.as_deref(), Some("old result"));
        // The next handle must not collide with recovered ids.
        let next = reg2.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(next, 43, "next id re-indexed above recovered d00042");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_rejects_running_and_unknown() {
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
            registry: ToolRegistry::with_tools(None, None, None, None, None),
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
                goals_dir: None,
                planning_interval: 0,
            },
            delegates: Arc::new(DelegateRegistry::default()),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
        };

        // Unknown id → clear error.
        let err = tool
            .run(&json!({"action": "resume", "id": "d99999", "task": "continue"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such delegate id"), "err: {err}");

        // Running sub-agent → refused (must finish first).
        let reg = Arc::new(DelegateRegistry::default());
        reg.insert(DelegateState {
            id: "d00003".into(),
            name: "busy".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
        })
        .await;
        let tool = DelegateTool {
            delegates: reg,
            ..tool
        };
        let err = tool
            .run(&json!({"action": "resume", "id": "d00003", "task": "continue"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("still running"), "err: {err}");
    }

    #[tokio::test]
    async fn spawn_unknown_subagent_type_lists_available() {
        let tool = DelegateTool {
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            ..DelegateTool::new(
                Arc::new(
                    crate::llm::openai::OpenAiProvider::with_fallbacks(
                        crate::llm::openai::OpenAiConfig {
                            base_url: "http://localhost:1/v1".into(),
                            api_key: "x".into(),
                        },
                        vec![],
                    )
                    .unwrap(),
                ),
                ToolRegistry::with_tools(None, None, None, None, None),
                AgentConfig {
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
                    goals_dir: None,
                    planning_interval: 0,
                },
                None,
                None,
            )
        };
        let err = tool
            .run(&json!({"action": "spawn", "task": "t", "subagent_type": "nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such sub-agent type 'nope'"), "err: {err}");
        assert!(err.contains("available:"), "err: {err}");
    }

    #[test]
    fn apply_subagent_type_appends_prompt_and_filters_tools() {
        // A registry with many tools; the type allows only one.
        let mut reg = ToolRegistry::with_tools(None, None, None, None, None);
        let before = reg.specs().len();
        assert!(before > 1, "expected several tools, got {before}");
        let def = subagent::AgentDefinition {
            name: "focused".into(),
            description: "only read_file".into(),
            tools: Some(vec!["read_file".into()]),
            prompt: "You only use read_file.".into(),
        };
        let config = AgentConfig {
            model: "test".into(),
            max_steps: 3,
            prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                crate::harness::PromptState {
                    system_prompt: "base prompt".into(),
                    skills: vec![],
                    fingerprint: None,
                },
            )),
            max_inline_skills: 0,
            sync_dir: None,
            max_context_messages: 0,
            max_context_tokens: 0,
            goals: None,
            goals_dir: None,
            planning_interval: 0,
        };
        let parent_arc = config.prompt_state.clone();

        let out = DelegateTool::apply_subagent_type(&config, &mut reg, &def);

        // Prompt suffix appended to a deep copy.
        let out_prompt = out.prompt_state.read().unwrap().system_prompt.clone();
        assert_eq!(out_prompt, "base prompt\n\nYou only use read_file.");
        // Parent prompt state untouched (deep copy, shared Arc intact).
        assert_eq!(parent_arc.read().unwrap().system_prompt, "base prompt");
        // Tool allowlist applied: only read_file remains.
        let names: Vec<String> = reg.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["read_file"], "all other tools dropped");
    }
}
