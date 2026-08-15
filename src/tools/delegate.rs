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
    /// Model the sub-agent ran with, when `spawn` overrode it
    /// (rlm.find_models-style per-sub-task model pick).
    #[serde(default)]
    pub model: Option<String>,
    /// Messages the sub-agent sent to its parent via `child_message`
    /// while running (rlm `agent_message.send` semantics); the parent
    /// reads them with `delegate status`. Bounded to the most recent
    /// [`MAX_MESSAGES`].
    #[serde(default)]
    pub messages: Vec<String>,
    /// Parent→sub-agent queued messages (`delegate send`); consumed by
    /// the sub-agent's `check_parent_messages` tool.
    #[serde(default)]
    pub inbox: Vec<String>,
    /// Cumulative token usage reported by the provider for the run
    /// (None when the provider does not report usage).
    #[serde(default)]
    pub tokens: Option<crate::llm::provider::TokenUsage>,
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

    /// Append a message the sub-agent sent to its parent (via the
    /// `child_message` tool). Bounded to the most recent
    /// [`MAX_MESSAGES`]; unknown ids are ignored (e.g. the
    /// record was deleted mid-run).
    async fn push_message(&self, id: &str, msg: &str) {
        self.update(id, |st| {
            st.messages.push(msg.to_string());
            if st.messages.len() > MAX_MESSAGES {
                let excess = st.messages.len() - MAX_MESSAGES;
                st.messages.drain(..excess);
            }
        })
        .await;
    }

    /// Queue a message from the parent to the sub-agent (the parent→
    /// child direction of rlm `agent_message.send`); the sub-agent
    /// consumes it with `check_parent_messages`. Unknown ids are
    /// ignored (e.g. the record was deleted mid-run).
    async fn send_to_child(&self, id: &str, msg: &str) {
        self.update(id, |st| {
            st.inbox.push(msg.to_string());
            if st.inbox.len() > MAX_MESSAGES {
                let excess = st.inbox.len() - MAX_MESSAGES;
                st.inbox.drain(..excess);
            }
        })
        .await;
    }

    /// Take (and clear) the parent's queued messages for a sub-agent,
    /// returning them oldest-first. The `check_parent_messages` tool
    /// calls this, so a message is consumed exactly once.
    async fn take_inbox(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.update(id, |st| std::mem::swap(&mut out, &mut st.inbox))
            .await;
        out
    }

    /// Move the parent's queued messages from `from` to `to` — the
    /// resume-continuation semantics: the new handle keeps seeing the
    /// parent's messages and the old record never consumes them twice.
    async fn transfer_inbox(&self, from: &str, to: &str) {
        let mut guard = self.states.lock().await;
        let mut msgs = Vec::new();
        if let Some(old) = guard.get_mut(from) {
            std::mem::swap(&mut msgs, &mut old.inbox);
        }
        if let Some(new) = guard.get_mut(to) {
            new.inbox = msgs;
        }
        if let Some(dir) = &self.dir {
            if let Some(old) = guard.get(from) {
                let _ = old.persist(dir);
            }
            if let Some(new) = guard.get(to) {
                let _ = new.persist(dir);
            }
        }
    }
}

/// Cap on queued child→parent and parent→child messages (keeps
/// `delegate status` / `check_parent_messages` from growing without
/// bound when a chatty sub-agent streams updates).
const MAX_MESSAGES: usize = 50;

/// Tool injected into every sub-agent's tool set so it can send
/// messages to its parent while running (the rlm `agent_message.send`
/// semantics): the text lands in the delegate's `messages` queue,
/// which the parent reads via `delegate status`.
pub struct ChildMessageTool {
    delegates: Arc<DelegateRegistry>,
    id: String,
}

impl ChildMessageTool {
    pub fn new(delegates: Arc<DelegateRegistry>, id: String) -> Self {
        Self { delegates, id }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for ChildMessageTool {
    fn name(&self) -> &str {
        "child_message"
    }

    fn description(&self) -> &str {
        "Send a message to the parent agent. Only available to sub-agents; \
         the parent reads your messages with delegate status. Use it for \
         progress updates, intermediate findings, or anything the parent \
         should know before you finish."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "the message to send to the parent"
                }
            },
            "required": ["text"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .context("child_message requires a 'text' string")?;
        self.delegates.push_message(&self.id, text).await;
        Ok(ToolOutput {
            text: "message sent to parent".to_string(),
        })
    }
}

/// Tool injected into every sub-agent's tool set so it can read
/// messages the parent sent while it runs (`delegate send` — the
/// parent→child direction of rlm `agent_message.send`). Consumes the
/// queued messages (each is delivered exactly once). The sub-agent's
/// system prompt tells it to check at the start of every step.
pub struct CheckParentMessagesTool {
    delegates: Arc<DelegateRegistry>,
    id: String,
}

impl CheckParentMessagesTool {
    pub fn new(delegates: Arc<DelegateRegistry>, id: String) -> Self {
        Self { delegates, id }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for CheckParentMessagesTool {
    fn name(&self) -> &str {
        "check_parent_messages"
    }

    fn description(&self) -> &str {
        "Check for messages your parent sent you (via delegate send). \
         Only available to sub-agents. Call this at the start of every \
         step and follow any instructions in the messages. Takes no \
         arguments; returns the waiting messages, or a note that there \
         are none."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn run(&self, _args: &Value) -> anyhow::Result<ToolOutput> {
        let inbox = self.delegates.take_inbox(&self.id).await;
        if inbox.is_empty() {
            return Ok(ToolOutput {
                text: "no messages from parent".to_string(),
            });
        }
        Ok(ToolOutput {
            text: format!(
                "messages from parent ({n}):\n{}",
                inbox
                    .iter()
                    .enumerate()
                    .map(|(i, m)| format!("{}. {m}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
                n = inbox.len()
            ),
        })
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
    /// Nesting depth of this tool instance (0 = top-level agent; a
    /// depth-1 sub-agent's delegate tool is refused further nesting).
    depth: u32,
    /// Optional progress reporter — the ACP host (brainflow) wires this
    /// to stream sub-agent lifecycle events while a batch runs, so the
    /// decision canvas can show branches as they start and finish.
    progress: Option<Arc<dyn Fn(DelegateProgress) + Send + Sync>>,
}

/// One sub-agent lifecycle event for a live delegate batch. `step` is
/// the 1-based ordinal of this sub-agent within the current batch and
/// `total` the batch size (both 1 for a single `spawn`).
#[derive(Debug, Clone)]
pub struct DelegateProgress {
    pub id: String,
    pub name: String,
    /// "running" | "completed" | "failed"
    pub status: &'static str,
    pub step: usize,
    pub total: usize,
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
            depth: 0,
            progress: None,
        }
    }

    /// A copy of this tool at another nesting depth — installed into a
    /// sub-agent's tool registry in place of the shared parent
    /// instance, so a delegated sub-agent's delegate calls are
    /// governed by `depth + 1` (and refused once `MAX_SUBAGENT_DEPTH`
    /// is reached).
    fn with_depth(&self, depth: u32) -> Self {
        Self {
            provider: self.provider.clone(),
            registry: self.registry.clone_shallow(),
            config: self.config.clone(),
            delegates: self.delegates.clone(),
            agents: self.agents.clone(),
            description: std::sync::OnceLock::new(),
            depth,
            progress: self.progress.clone(),
        }
    }

    /// A copy of this tool wired to a progress reporter — used by the
    /// ACP server so a live delegate batch streams sub-agent lifecycle
    /// events to the host (brainflow decision canvas).
    pub fn with_progress(
        self,
        progress: Arc<dyn Fn(DelegateProgress) + Send + Sync>,
    ) -> Self {
        Self {
            progress: Some(progress),
            ..self
        }
    }

    /// Refuse to delegate when this instance sits at (or beyond) the
    /// nesting limit.
    fn check_depth(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.depth < MAX_SUBAGENT_DEPTH,
            "sub-agent nesting depth limit ({MAX_SUBAGENT_DEPTH}) reached: \
             a depth-{} sub-agent cannot delegate further",
            self.depth
        );
        Ok(())
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
    /// An optional `model` overrides the sub-agent's model (the
    /// rlm.find_models idea: pick a model per sub-task) and is
    /// recorded on the handle.
    // Internal spawn plumbing — many knobs, all explicit (task, name,
    // resume, type, model, inbox, per-sub-agent condenser overrides).
    #[allow(clippy::too_many_arguments)]
    async fn spawn_subagent(
        &self,
        task: String,
        name: Option<String>,
        resume_from: Option<String>,
        subagent_type: Option<&str>,
        model: Option<&str>,
        inbox: Vec<String>,
        max_context_messages: Option<usize>,
        max_context_tokens: Option<usize>,
    ) -> anyhow::Result<String> {
        let mut sub_tools = self.registry.clone_shallow();
        // Nesting depth: the sub-agent's delegate tool is a depth+1
        // copy of this instance (a delegated sub-agent can then not
        // delegate again, per prime-agent's RLM_MAX_DEPTH default).
        sub_tools.unregister("delegate");
        sub_tools.register(std::sync::Arc::new(self.with_depth(self.depth + 1)));
        let mut config = if let Some(ty) = subagent_type {
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
        if let Some(m) = model {
            config.model = m.to_string();
        }
        // Per-sub-agent condenser override (OpenHands
        // definition.condenser): the sub-agent's context budget can
        // differ from the parent's global max_context_* settings.
        if let Some(n) = max_context_messages {
            config.max_context_messages = n;
        }
        if let Some(n) = max_context_tokens {
            config.max_context_tokens = n;
        }
        let name = match name {
            Some(n) => n,
            None => task.chars().take(40).collect::<String>(),
        };
        let id = format!("d{:05}", self.delegates.next.fetch_add(1, Ordering::SeqCst));
        let ts = crate::session::chrono_like_timestamp();
        // The sub-agent can message its parent while running (rlm
        // agent_message.send semantics). Registered after the type's
        // allowlist filter so it survives every sub-agent type.
        sub_tools.register(Arc::new(ChildMessageTool::new(
            self.delegates.clone(),
            id.clone(),
        )));
        sub_tools.register(Arc::new(CheckParentMessagesTool::new(
            self.delegates.clone(),
            id.clone(),
        )));
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
                model: model.map(|m| m.to_string()),
                messages: Vec::new(),
                inbox,
                tokens: None,
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
             \
             [COMMUNICATION RULES — sub-agent]\n\
             - Your parent can send you messages while you work\n\
               (they appear via the check_parent_messages tool).\n\
             - Call check_parent_messages at the start of EVERY step\n\
               and follow any instructions in the messages.\n\
             - You can message your parent any time with the\n\
               child_message tool (the parent reads them via delegate\n\
               status).\n\
             [END COMMUNICATION RULES]\n\
             \n\
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
                            st.tokens = res.tokens;
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

/// How many sub-agents may run at once (OpenHands' `max_children`,
/// hard-capped at 5). A spawn or delegate batch that would exceed the
/// cap is rejected up front so a runaway delegation can't exhaust
/// resources.
const MAX_ACTIVE_SUBAGENTS: usize = 5;

/// How deep sub-agent nesting may go (prime-agent's `RLM_MAX_DEPTH`,
/// default 1): a sub-agent delegated by the top-level agent may use
/// tools, but a delegated sub-agent cannot delegate again. Each
/// `DelegateTool` instance carries its own depth (0 = top level);
/// spawn/delegate/resume refuse to go deeper.
const MAX_SUBAGENT_DEPTH: u32 = 1;

/// Validate and normalize the optional `name` arg of `delegate spawn`
/// (mirrors prime-agent's `normalizeRequestedRlmSubagentSessionName`:
/// trim, non-empty, at most 64 chars). `None` keeps the default name
/// (first 40 chars of the task).
fn spawn_name(args: &serde_json::Value) -> anyhow::Result<Option<String>> {
    let Some(v) = args.get("name") else {
        return Ok(None);
    };
    let raw = v
        .as_str()
        .context("delegate spawn 'name' must be a string")?;
    let trimmed = raw.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "delegate spawn 'name' must not be empty"
    );
    anyhow::ensure!(
        trimmed.chars().count() <= 64,
        "delegate spawn 'name' must be at most 64 characters"
    );
    Ok(Some(trimmed.to_string()))
}

/// Blocking batch collect (OpenHands `delegate` semantics): poll the
/// given sub-agent ids until every one has left `Running` or the
/// timeout elapses, then format the aggregated results. Still-running
/// sub-agents are reported as such (their background tasks keep going
/// — poll with `action=result` later), so the parent's turn is never
/// stuck forever on a hung sub-agent.
async fn wait_for_delegates(
    delegates: &DelegateRegistry,
    ids: &[String],
    step_of: &[(String, usize, String)],
    timeout_secs: u64,
    progress: Option<&(dyn Fn(DelegateProgress) + Send + Sync)>,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    // Report a terminal state at most once per sub-agent.
    let mut reported: std::collections::HashSet<&str> = std::collections::HashSet::new();
    loop {
        let states = delegates.snapshot().await;
        // Progress bridge: each sub-agent that reached a terminal state
        // (completed/failed) since the last poll gets a `completed` /
        // `failed` event so the host canvas can update live.
        for (id, step, name) in step_of {
            if reported.contains(id.as_str()) {
                continue;
            }
            if let (Some(st), Some(cb)) = (
                states.iter().find(|s| &s.id == id),
                progress,
            ) && st.status != DelegateStatus::Running
            {
                cb(DelegateProgress {
                    id: id.clone(),
                    name: name.clone(),
                    status: st.status.as_str(),
                    step: *step,
                    total: step_of.len(),
                });
                reported.insert(id.as_str());
            }
        }
        let all_done = ids.iter().all(|id| {
            states
                .iter()
                .any(|s| &s.id == id && s.status != DelegateStatus::Running)
        });
        if all_done || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let states = delegates.snapshot().await;
    let mut text = format!("delegate batch ({} tasks):", ids.len());
    for id in ids {
        let Some(st) = states.iter().find(|s| &s.id == id) else {
            text.push_str(&format!("\n- {id} [missing]"));
            continue;
        };
        let steps = st
            .steps
            .map(|n| format!(" ({n} steps)"))
            .unwrap_or_default();
        let tokens = st
            .tokens
            .map(|t| {
                format!(
                    " ({} prompt + {} completion tokens)",
                    t.prompt_tokens, t.completion_tokens
                )
            })
            .unwrap_or_default();
        match &st.status {
            DelegateStatus::Completed => {
                let report = st
                    .result
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "\n- {id} [completed] {}{}{}{}",
                    st.name, steps, tokens, report
                ));
            }
            DelegateStatus::Failed => {
                text.push_str(&format!(
                    "\n- {id} [failed] {} — {}",
                    st.name,
                    st.error.as_deref().unwrap_or("unknown error")
                ));
            }
            DelegateStatus::Running => {
                text.push_str(&format!(
                    "\n- {id} [running] {} — still running after {timeout_secs}s (spawned {}); poll action=result later",
                    st.name, st.started_ts
                ));
            }
        }
    }
    Ok(text)
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
                 you work; optional 'name' ≤64 chars, trimmed, non-empty), 'delegate' \
                 (tasks: [{task, name?, subagent_type?, model?}, ...] — spawn the \
                 whole batch concurrently and block until every sub-agent \
                 finishes, returning aggregated results in one call; optional \
                 'timeout' seconds, default 600), 'status' (id — running/completed/failed), 'result' \
                 (id — the sub-agent's final summary once completed), 'resume' \
                 (id + task — continue a finished/failed sub-agent with a new \
                 instruction: its original task and final report are fed back \
                 as context, and a fresh handle is returned), 'list' \
                 (all sub-agents), 'delete' (id — remove a finished \
                 sub-agent's record and free its handle), 'metrics' \
                 (per-sub-agent token usage with a grand total). At most 5 sub-agents \
                 may run at once (a spawn or batch beyond the cap is rejected); \
                 sub-agent nesting is capped at depth 1 (a delegated sub-agent \
                 cannot delegate further). Use for parallel \
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
                    "enum": ["spawn", "delegate", "status", "result", "resume", "list", "metrics", "delete", "send"],
                    "description": "spawn (default): start one sub-agent; delegate: spawn a batch of tasks concurrently and block until they finish (aggregated results); status: poll one; result: collect one's summary; resume: continue a finished/failed sub-agent with a new instruction (returns a fresh handle); list: enumerate all; metrics: per-sub-agent token usage with a grand total; delete: remove a finished sub-agent's record; send: queue a message to a running sub-agent (it reads it via check_parent_messages)"
                },
                "task": {
                    "type": "string",
                    "description": "the task for the sub-agent (required for spawn and resume)"
                },
                "tasks": {
                    "type": "array",
                    "description": "batch of sub-agent tasks for action=delegate: [{task, name?, subagent_type?, model?, max_context_messages?, max_context_tokens?}, ...] — spawned concurrently and collected together",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task": {"type": "string"},
                            "name": {"type": "string"},
                            "subagent_type": {"type": "string"},
                            "model": {"type": "string"},
                            "max_context_messages": {"type": "integer"},
                            "max_context_tokens": {"type": "integer"}
                        }
                    }
                },
                "timeout": {
                    "type": "integer",
                    "description": "seconds to wait for action=delegate's batch (default 600; on timeout still-running sub-agents are reported as such)"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Optional sub-agent type name (system-prompt suffix + tool allowlist)",
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the sub-agent (rlm.find_models-style per-sub-task model pick; the handle records which model ran)",
                },
                "max_context_messages": {
                    "type": "integer",
                    "description": "Per-sub-agent condenser override: max messages kept in the sub-agent's context before compaction (OpenHands definition.condenser); defaults to the parent's global setting"
                },
                "max_context_tokens": {
                    "type": "integer",
                    "description": "Per-sub-agent condenser override: max context tokens for the sub-agent; defaults to the parent's global setting"
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
                self.check_depth()?;
                let task = args
                    .get("task")
                    .and_then(Value::as_str)
                    .context("delegate spawn requires a 'task' string")?
                    .to_string();
                let task_for_progress = task.clone();
                let active = self
                    .delegates
                    .snapshot()
                    .await
                    .iter()
                    .filter(|s| s.status == DelegateStatus::Running)
                    .count();
                anyhow::ensure!(
                    active < MAX_ACTIVE_SUBAGENTS,
                    "at most {MAX_ACTIVE_SUBAGENTS} sub-agents may run at once \
                     ({active} running now); collect or delete finished ones first"
                );
                let id = self
                    .spawn_subagent(
                        task,
                        spawn_name(args)?,
                        None,
                        args.get("subagent_type").and_then(Value::as_str),
                        args.get("model").and_then(Value::as_str),
                        Vec::new(),
                        args.get("max_context_messages")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize),
                        args.get("max_context_tokens")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize),
                    )
                    .await?;
                if let Some(cb) = &self.progress {
                    cb(DelegateProgress {
                        id: id.clone(),
                        name: spawn_name(args)?
                            .unwrap_or_else(|| task_for_progress.clone()),
                        status: "running",
                        step: 1,
                        total: 1,
                    });
                }
                Ok(ToolOutput {
                    text: format!(
                        "delegate '{id}' spawned (status: running). Keep working; poll with action=status, collect with action=result."
                    ),
                })
            }
            "delegate" => {
                self.check_depth()?;
                // OpenHands-style blocking batch: spawn every task
                // concurrently (each sub-agent runs in its own tokio
                // task) and wait for them all to finish, returning the
                // aggregated results. `timeout` (seconds, default 600)
                // bounds the wait — on timeout the still-running ones
                // are reported as such instead of erroring.
                let tasks = args
                    .get("tasks")
                    .and_then(Value::as_array)
                    .context("delegate requires a 'tasks' array of {task, ...} objects")?;
                if tasks.is_empty() {
                    anyhow::bail!("delegate 'tasks' must not be empty");
                }
                if tasks.len() > MAX_ACTIVE_SUBAGENTS {
                    anyhow::bail!(
                        "delegate batch has {} tasks but at most {MAX_ACTIVE_SUBAGENTS} \
                         sub-agents may run at once; split into smaller batches",
                        tasks.len()
                    );
                }
                let active = self
                    .delegates
                    .snapshot()
                    .await
                    .iter()
                    .filter(|s| s.status == DelegateStatus::Running)
                    .count();
                anyhow::ensure!(
                    tasks.len() + active <= MAX_ACTIVE_SUBAGENTS,
                    "delegate batch would run {} sub-agents but at most {MAX_ACTIVE_SUBAGENTS} \
                     may be active at once ({active} already running); collect or delete \
                     finished ones first",
                    tasks.len() + active
                );
                let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(600);
                let mut ids = Vec::new();
                let mut step_of: Vec<(String, usize, String)> = Vec::new();
                let batch_total = tasks.len();
                for t in tasks {
                    let task = t
                        .get("task")
                        .and_then(Value::as_str)
                        .context("each delegate task needs a 'task' string")?
                        .to_string();
                    let name = t
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| n.trim().to_string())
                        .filter(|n| !n.is_empty())
                        .filter(|n| n.chars().count() <= 64);
                    let step = ids.len() + 1;
                    if let Some(cb) = &self.progress {
                        cb(DelegateProgress {
                            id: format!("pending-{step}"),
                            name: name.clone().unwrap_or_else(|| task.clone()),
                            status: "running",
                            step,
                            total: batch_total,
                        });
                    }
                    let id = self
                        .spawn_subagent(
                            task,
                            name,
                            None,
                            t.get("subagent_type").and_then(Value::as_str),
                            t.get("model").and_then(Value::as_str),
                            Vec::new(),
                            t.get("max_context_messages")
                                .and_then(Value::as_u64)
                                .map(|v| v as usize),
                            t.get("max_context_tokens")
                                .and_then(Value::as_u64)
                                .map(|v| v as usize),
                        )
                        .await?;
                    ids.push(id.clone());
                    step_of.push((id, step, format!("pending-{step}")));
                }
                let text = wait_for_delegates(&self.delegates, &ids, &step_of, timeout, self.progress.as_deref()).await?;
                Ok(ToolOutput { text })
            }
            "resume" => {
                self.check_depth()?;
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
                        Some(st.name.clone()),
                        Some(st.id.clone()),
                        args.get("subagent_type").and_then(Value::as_str),
                        args.get("model").and_then(Value::as_str),
                        Vec::new(),
                        args.get("max_context_messages")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize),
                        args.get("max_context_tokens")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize),
                    )
                    .await?;
                // The parent's queued messages transfer to the new
                // handle (the continuation keeps seeing them) and are
                // cleared from the old record so they are never
                // consumed twice.
                self.delegates.transfer_inbox(&st.id, &new_id).await;
                Ok(ToolOutput {
                    text: format!(
                        "delegate '{new_id}' spawned as a continuation of '{}' (status: running). Poll with action=status, collect with action=result.",
                        st.id
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
                let mut text = match st.status {
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
                };
                if !st.messages.is_empty() {
                    text.push_str("\nmessages from sub-agent:");
                    for m in &st.messages {
                        text.push_str(&format!("\n- {m}"));
                    }
                }
                if let Some(t) = st.tokens {
                    text.push_str(&format!(
                        "\nusage: {} prompt + {} completion tokens",
                        t.prompt_tokens, t.completion_tokens
                    ));
                }
                Ok(ToolOutput { text })
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
                    let model = st
                        .model
                        .as_deref()
                        .map(|m| format!(" (model: {m})"))
                        .unwrap_or_default();
                    let tokens = st
                        .tokens
                        .map(|t| {
                            format!(
                                " ({} prompt + {} completion tokens)",
                                t.prompt_tokens, t.completion_tokens
                            )
                        })
                        .unwrap_or_default();
                    text.push_str(&format!(
                        "\n- {} [{}] — {}{}{}{}{}",
                        st.id,
                        st.status.as_str(),
                        st.name,
                        st.resume_from
                            .as_deref()
                            .map(|r| format!(" (resumed from {r})"))
                            .unwrap_or_default(),
                        steps,
                        model,
                        tokens
                    ));
                }
                Ok(ToolOutput { text })
            }
            "metrics" => {
                // OpenHands usage_to_metrics view, keyed per delegate
                // id: one line per sub-agent with its own token usage,
                // plus a grand total so the parent can see the whole
                // delegation bill at a glance (each sub-agent's usage
                // is recorded once, on completion — replace, never
                // merged, so resumed handles don't double-count).
                let states = self.delegates.snapshot().await;
                let with_usage: Vec<&DelegateState> =
                    states.iter().filter(|s| s.tokens.is_some()).collect();
                if with_usage.is_empty() {
                    return Ok(ToolOutput {
                        text: "no sub-agent token usage recorded yet (delegates report usage once they finish)"
                            .to_string(),
                    });
                }
                let mut total_prompt = 0u64;
                let mut total_completion = 0u64;
                let mut text = format!("delegate token usage ({} sub-agents):", with_usage.len());
                for st in with_usage {
                    let t = st.tokens.unwrap();
                    total_prompt += t.prompt_tokens;
                    total_completion += t.completion_tokens;
                    text.push_str(&format!(
                        "\n- {id} {name}: {} prompt + {} completion",
                        t.prompt_tokens,
                        t.completion_tokens,
                        id = st.id,
                        name = st.name,
                    ));
                }
                text.push_str(&format!(
                    "\ntotal: {total_prompt} prompt + {total_completion} completion"
                ));
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
            "send" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("send requires 'id'"))?;
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("send requires 'text'"))?;
                self.delegates.send_to_child(id, text).await;
                Ok(ToolOutput {
                    text: format!(
                        "message queued for delegate '{id}'; it will see it on its next check_parent_messages call"
                    ),
                })
            }
            _ => anyhow::bail!(
                "unknown action '{action}' (spawn|delegate|status|result|resume|list|metrics|delete|send)"
            ),
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: Arc::new(DelegateRegistry::default()),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };
        assert_eq!(tool.name(), "delegate");
        let p = tool.parameters();
        let actions = p["properties"]["action"]["enum"].as_array().unwrap();
        let names: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "spawn", "delegate", "status", "result", "resume", "list", "metrics", "delete",
                "send"
            ]
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
                    model: None,
                    messages: Vec::new(),
                    inbox: Vec::new(),
                    tokens: None,
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
    async fn wait_for_delegates_returns_aggregate_when_all_done() {
        let reg = DelegateRegistry::default();
        let mut ids = Vec::new();
        for i in 0..2 {
            let id = format!("d{i:05}");
            reg.insert(DelegateState {
                id: id.clone(),
                name: format!("task-{i}"),
                status: DelegateStatus::Completed,
                started_ts: "t".into(),
                finished_ts: Some("t1".into()),
                result: Some(format!("answer-{i}")),
                error: None,
                task: format!("task-{i}"),
                resume_from: None,
                steps: Some(1),
                model: None,
                messages: Vec::new(),
                inbox: Vec::new(),
                tokens: None,
            })
            .await;
            ids.push(id);
        }
        let step_of: Vec<(String, usize, String)> =
            ids.iter().enumerate().map(|(i, id)| (id.clone(), i + 1, format!("task-{i}"))).collect();
        let text = wait_for_delegates(&reg, &ids, &step_of, 2, None).await.unwrap();
        assert!(text.contains("delegate batch (2 tasks)"), "got: {text}");
        assert!(
            text.contains("d00000 [completed] task-0 (1 steps) — answer-0"),
            "got: {text}"
        );
        assert!(
            text.contains("d00001 [completed] task-1 (1 steps) — answer-1"),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn wait_for_delegates_reports_progress_events() {
        let reg = DelegateRegistry::default();
        let mut ids = Vec::new();
        for i in 0..2 {
            let id = format!("d{i:05}");
            reg.insert(DelegateState {
                id: id.clone(),
                name: format!("task-{i}"),
                status: DelegateStatus::Completed,
                started_ts: "t".into(),
                finished_ts: Some("t1".into()),
                result: Some(format!("answer-{i}")),
                error: None,
                task: format!("task-{i}"),
                resume_from: None,
                steps: Some(1),
                model: None,
                messages: Vec::new(),
                inbox: Vec::new(),
                tokens: None,
            })
            .await;
            ids.push(id);
        }
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let events2 = events.clone();
        let cb = move |p: DelegateProgress| {
            events2
                .lock()
                .unwrap()
                .push(format!("{}:{}:{}", p.id, p.status, p.step));
        };
        let step_of: Vec<(String, usize, String)> =
            ids.iter().enumerate().map(|(i, id)| (id.clone(), i + 1, format!("task-{i}"))).collect();
        let text =
            wait_for_delegates(&reg, &ids, &step_of, 2, Some(&cb)).await.unwrap();
        assert!(text.contains("delegate batch (2 tasks)"), "got: {text}");
        let got = events.lock().unwrap().clone();
        // Both already terminal on the first poll → exactly one event each,
        // with correct step ordinals and terminal statuses.
        assert_eq!(got.len(), 2, "events: {got:?}");
        assert!(got.iter().any(|e| e == "d00000:completed:1"), "events: {got:?}");
        assert!(got.iter().any(|e| e == "d00001:completed:2"), "events: {got:?}");
    }

    #[tokio::test]
    async fn wait_for_delegates_reports_still_running_after_timeout() {
        let reg = DelegateRegistry::default();
        let mut ids = Vec::new();
        // One finishes, one stays Running forever.
        for i in 0..2 {
            let id = format!("d{i:05}");
            reg.insert(DelegateState {
                id: id.clone(),
                name: format!("task-{i}"),
                status: if i == 0 {
                    DelegateStatus::Completed
                } else {
                    DelegateStatus::Running
                },
                started_ts: "t".into(),
                finished_ts: if i == 0 { Some("t1".into()) } else { None },
                result: if i == 0 {
                    Some("answer-0".into())
                } else {
                    None
                },
                error: None,
                task: format!("task-{i}"),
                resume_from: None,
                steps: if i == 0 { Some(1) } else { None },
                model: None,
                messages: Vec::new(),
                inbox: Vec::new(),
                tokens: None,
            })
            .await;
            ids.push(id);
        }
        let step_of: Vec<(String, usize, String)> =
            ids.iter().enumerate().map(|(i, id)| (id.clone(), i + 1, format!("task-{i}"))).collect();
        // 1s timeout: must return promptly with the partial aggregate,
        // not hang.
        let text = wait_for_delegates(&reg, &ids, &step_of, 1, None).await.unwrap();
        assert!(
            text.contains("d00000 [completed] task-0 (1 steps) — answer-0"),
            "got: {text}"
        );
        assert!(
            text.contains("d00001 [running] task-1 — still running after 1s"),
            "got: {text}"
        );
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
        })
        .await;
        reg.update("d00001", |st| {
            st.status = DelegateStatus::Completed;
            st.result = Some("done".into());
            st.steps = Some(7);
            st.model = Some("model-x".into());
        })
        .await;
        let back: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00001").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back.steps, Some(7), "steps persisted to meta.json");
        assert_eq!(
            back.model.as_deref(),
            Some("model-x"),
            "model override persisted to meta.json"
        );

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
        assert_eq!(
            legacy.model, None,
            "legacy meta.json without model loads fine"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_rejects_bad_names_and_keeps_good_one() {
        let reg = Arc::new(DelegateRegistry::default());
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };

        // Empty (after trim) and >64-char names are rejected — the
        // rlm normalizeRequestedRlmSubagentSessionName semantics.
        let err = tool
            .run(&json!({"action": "spawn", "task": "t", "name": "   "}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must not be empty"));
        let long = "x".repeat(65);
        let err = tool
            .run(&json!({"action": "spawn", "task": "t", "name": long}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must be at most 64 characters"));

        // A valid name is trimmed and recorded on the handle.
        let ok = tool
            .run(&json!({"action": "spawn", "task": "t", "name": "  my sub-agent  "}))
            .await
            .unwrap();
        let id = ok
            .text
            .split('\'')
            .nth(1)
            .expect("spawn output carries the handle id")
            .to_string();
        let snap = tool.delegates.snapshot().await;
        let st = snap.iter().find(|s| s.id == id).unwrap();
        assert_eq!(st.name, "my sub-agent");

        // Resume keeps the original name on the continuation handle.
        // The spawned background task hits a dead provider port, so
        // mark the handle completed before resuming.
        reg.update(&id, |st| st.status = DelegateStatus::Completed)
            .await;
        let resume = tool
            .run(&json!({"action": "resume", "id": id, "task": "continue"}))
            .await
            .unwrap();
        let new_id = resume
            .text
            .split('\'')
            .nth(1)
            .expect("resume output carries the new handle id")
            .to_string();
        let snap = tool.delegates.snapshot().await;
        let st = snap.iter().find(|s| s.id == new_id).unwrap();
        assert_eq!(st.name, "my sub-agent", "name survives resume");
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
            model: Some("model-x".into()),
            messages: vec!["ping from child".into()],
            inbox: vec!["ping from parent".into()],
            tokens: Some(crate::llm::provider::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                prompt_cache_hit_tokens: 0,
            }),
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };
        let out = tool.run(&json!({"action": "list"})).await.unwrap();
        assert!(
            out.text.contains("(7 steps)"),
            "completed sub-agent shows step count: {}",
            out.text
        );
        assert!(
            out.text.contains("(model: model-x)"),
            "model override shown on the handle: {}",
            out.text
        );
        assert!(
            out.text.contains("(10 prompt + 5 completion tokens)"),
            "token usage shown on the handle: {}",
            out.text
        );
        assert!(!out.text.contains("(0 steps)"), "no '0 steps' shown");
        assert!(
            !out.text.contains("no steps") || !out.text.contains("(0 steps)"),
            "running sub-agent without steps shows no count"
        );
    }

    #[tokio::test]
    async fn metrics_aggregates_token_usage_per_subagent() {
        let reg = Arc::new(DelegateRegistry::default());
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "math-agent".into(),
            status: DelegateStatus::Completed,
            started_ts: "t0".into(),
            finished_ts: Some("t1".into()),
            result: Some("done".into()),
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: Some(2),
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: Some(crate::llm::provider::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 30,
                prompt_cache_hit_tokens: 0,
            }),
        })
        .await;
        reg.insert(DelegateState {
            id: "d00002".into(),
            name: "geo-agent".into(),
            status: DelegateStatus::Completed,
            started_ts: "t0".into(),
            finished_ts: Some("t1".into()),
            result: Some("done".into()),
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: Some(3),
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: Some(crate::llm::provider::TokenUsage {
                prompt_tokens: 200,
                completion_tokens: 40,
                prompt_cache_hit_tokens: 0,
            }),
        })
        .await;
        // A running sub-agent with no usage yet must not break the total.
        reg.insert(DelegateState {
            id: "d00003".into(),
            name: "slow".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };
        let out = tool.run(&json!({"action": "metrics"})).await.unwrap();
        assert!(
            out.text.contains("delegate token usage (2 sub-agents)"),
            "only sub-agents with usage counted: {}",
            out.text
        );
        assert!(
            out.text
                .contains("- d00001 math-agent: 100 prompt + 30 completion"),
            "per-sub-agent line: {}",
            out.text
        );
        assert!(
            out.text
                .contains("- d00002 geo-agent: 200 prompt + 40 completion"),
            "per-sub-agent line: {}",
            out.text
        );
        assert!(
            out.text.contains("total: 300 prompt + 70 completion"),
            "grand total sums both sub-agents: {}",
            out.text
        );
        assert!(
            !out.text.contains("d00003"),
            "running sub-agent excluded: {}",
            out.text
        );
    }

    /// Insert `n` sub-agents with the given status into `reg`. Uses a
    /// "seed-" id namespace so it can never collide with the spawn
    /// id generator's dNNNNN handles.
    async fn seed_states(reg: &DelegateRegistry, n: usize, status: DelegateStatus) {
        for i in 0..n {
            reg.insert(DelegateState {
                id: format!("seed-{i}"),
                name: format!("agent-{i}"),
                status,
                started_ts: "t0".into(),
                finished_ts: None,
                result: None,
                error: None,
                task: format!("task-{i}"),
                resume_from: None,
                steps: None,
                model: None,
                messages: Vec::new(),
                inbox: Vec::new(),
                tokens: None,
            })
            .await;
        }
    }

    fn test_tool(reg: Arc<DelegateRegistry>) -> DelegateTool {
        DelegateTool {
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg,
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        }
    }

    #[tokio::test]
    async fn spawn_rejects_when_max_active_reached() {
        // Five running sub-agents: the cap is hit, a sixth spawn fails.
        let reg = Arc::new(DelegateRegistry::default());
        seed_states(&reg, 5, DelegateStatus::Running).await;
        let tool = test_tool(reg.clone());
        let err = tool
            .run(&json!({"action": "spawn", "task": "one more"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("at most 5 sub-agents"),
            "cap message: {err}"
        );
        // A completed sub-agent frees a slot: 4 running + 1 completed
        // lets the next spawn through (it fails later on the dead
        // provider, but the cap check itself passes).
        let reg2 = Arc::new(DelegateRegistry::default());
        seed_states(&reg2, 4, DelegateStatus::Running).await;
        seed_states(&reg2, 1, DelegateStatus::Completed).await;
        let tool2 = test_tool(reg2.clone());
        let out = tool2
            .run(&json!({"action": "spawn", "task": "allowed"}))
            .await;
        assert!(
            out.is_ok(),
            "spawn with a free slot must pass the cap check: {out:?}"
        );
    }

    #[tokio::test]
    async fn spawn_accepts_condenser_overrides() {
        // Per-sub-agent condenser params (max_context_messages /
        // max_context_tokens) must flow through spawn and the
        // delegate batch without error. The sub-agent itself fails on
        // the dead provider, but the cap + parameter plumbing is what
        // we assert here.
        let reg = Arc::new(DelegateRegistry::default());
        let tool = test_tool(reg.clone());
        let out = tool
            .run(&json!({
                "action": "spawn",
                "task": "condenser test",
                "max_context_messages": 6,
                "max_context_tokens": 4000
            }))
            .await;
        assert!(
            out.is_ok(),
            "spawn with condenser overrides must pass: {out:?}"
        );
        let reg2 = Arc::new(DelegateRegistry::default());
        let tool2 = test_tool(reg2.clone());
        let tasks = json!([
            {"task": "t1", "max_context_messages": 4},
            {"task": "t2", "max_context_tokens": 2000}
        ]);
        let out2 = tool2
            .run(&json!({"action": "delegate", "tasks": tasks, "timeout": 2}))
            .await;
        assert!(
            out2.is_ok(),
            "batch with per-task condenser overrides must pass: {out2:?}"
        );
    }

    #[tokio::test]
    async fn nesting_depth_blocks_sub_sub_agents() {
        // prime-agent RLM_MAX_DEPTH semantics: a top-level (depth 0)
        // tool may spawn, but a depth-1 instance (what a sub-agent
        // holds) must refuse spawn/delegate/resume.
        let reg = Arc::new(DelegateRegistry::default());
        let tool = test_tool(reg.clone());
        // Depth 0: spawn is allowed (fails later on the dead provider,
        // but the depth check passes).
        let out = tool
            .run(&json!({"action": "spawn", "task": "top-level ok"}))
            .await;
        assert!(out.is_ok(), "depth-0 spawn must pass: {out:?}");
        // Depth 1: every delegating action is refused up front.
        let deep = tool.with_depth(1);
        let err = deep
            .run(&json!({"action": "spawn", "task": "nested"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("nesting depth limit (1) reached"),
            "spawn depth message: {err}"
        );
        let tasks = json!([{"task": "t1"}, {"task": "t2"}]);
        let err2 = deep
            .run(&json!({"action": "delegate", "tasks": tasks}))
            .await
            .unwrap_err();
        assert!(
            format!("{err2}").contains("nesting depth limit (1) reached"),
            "delegate depth message: {err2}"
        );
        let err3 = deep
            .run(&json!({"action": "resume", "id": "d00001", "task": "continue"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err3}").contains("nesting depth limit (1) reached"),
            "resume depth message: {err3}"
        );
    }

    #[tokio::test]
    async fn delegate_batch_rejects_when_over_max() {
        let reg = Arc::new(DelegateRegistry::default());
        let tool = test_tool(reg.clone());
        let tasks: Vec<serde_json::Value> = (0..6)
            .map(|i| json!({"task": format!("task-{i}")}))
            .collect();
        let err = tool
            .run(&json!({"action": "delegate", "tasks": tasks}))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("at most 5 sub-agents"),
            "batch cap message: {err}"
        );
        // Already-running sub-agents eat into the batch budget: 3
        // running + a 3-task batch is fine, but 3 running + 4 tasks is
        // rejected even though 4 <= 5.
        let reg2 = Arc::new(DelegateRegistry::default());
        seed_states(&reg2, 3, DelegateStatus::Running).await;
        let tool2 = test_tool(reg2.clone());
        let ok_tasks: Vec<serde_json::Value> = (0..2)
            .map(|i| json!({"task": format!("task-{i}")}))
            .collect();
        let out = tool2
            .run(&json!({"action": "delegate", "tasks": ok_tasks}))
            .await;
        assert!(
            out.is_ok(),
            "3 running + 2-task batch is within cap: {out:?}"
        );
        let big_tasks: Vec<serde_json::Value> = (0..3)
            .map(|i| json!({"task": format!("task-{i}")}))
            .collect();
        let err2 = tool2
            .run(&json!({"action": "delegate", "tasks": big_tasks}))
            .await
            .unwrap_err();
        assert!(
            format!("{err2}").contains("at most 5"),
            "running + batch combined cap: {err2}"
        );
    }

    #[tokio::test]
    async fn child_messages_are_bounded_persisted_and_shown_in_status() {
        let dir = std::env::temp_dir().join(format!("sigil-delegate-msg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = Arc::new(DelegateRegistry::new(Some(dir.clone())));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "chatty".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
        })
        .await;
        // A chatty sub-agent streams more than the cap.
        for i in 0..60 {
            reg.push_message("d00001", &format!("msg {i:02}")).await;
        }
        // Bounded to the most recent MAX_CHILD_MESSAGES.
        let snap = reg.snapshot().await;
        let st = snap.iter().find(|s| s.id == "d00001").unwrap();
        assert_eq!(st.messages.len(), 50, "queue is bounded");
        assert_eq!(st.messages.first().map(String::as_str), Some("msg 10"));
        assert_eq!(st.messages.last().map(String::as_str), Some("msg 59"));
        // Persisted to meta.json.
        let back: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00001").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back.messages.len(), 50, "messages persisted to meta.json");
        // Unknown ids are ignored (record deleted mid-run etc).
        reg.push_message("d99999", "nobody home").await;
        // `delegate status` surfaces the messages to the parent.
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
            registry: ToolRegistry::default(),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };
        let out = tool
            .run(&json!({
                "action": "status",
                "id": "d00001",
            }))
            .await
            .unwrap();
        assert!(
            out.text.contains("messages from sub-agent"),
            "status shows the messages section: {}",
            out.text
        );
        assert!(
            out.text.contains("msg 59"),
            "status shows the message text: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parent_messages_are_queued_persisted_and_consumed_once() {
        let dir = std::env::temp_dir().join(format!("sigil-delegate-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = Arc::new(DelegateRegistry::new(Some(dir.clone())));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "child".into(),
            status: DelegateStatus::Running,
            started_ts: "t0".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
        })
        .await;
        // The parent queues two messages (the rlm parent→child send).
        reg.send_to_child("d00001", "first instruction").await;
        reg.send_to_child("d00001", "second instruction").await;
        // Persisted to meta.json.
        let back: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00001").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back.inbox.len(), 2, "inbox persisted to meta.json");
        // The sub-agent's check consumes them exactly once, oldest-first.
        let taken = reg.take_inbox("d00001").await;
        assert_eq!(taken, vec!["first instruction", "second instruction"]);
        let again = reg.take_inbox("d00001").await;
        assert!(again.is_empty(), "messages are consumed exactly once");
        // The delegate `send` action surfaces the queueing message.
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
            registry: ToolRegistry::default(),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
        };
        let out = tool
            .run(&json!({"action": "send", "id": "d00001", "text": "watch out"}))
            .await
            .unwrap();
        assert!(
            out.text.contains("message queued for delegate 'd00001'"),
            "send action reports the queue: {}",
            out.text
        );
        assert_eq!(
            reg.take_inbox("d00001").await,
            vec!["watch out"],
            "the queued message reaches the sub-agent"
        );
        // Unknown ids are ignored (no error).
        reg.send_to_child("d99999", "nobody home").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_transfers_parent_inbox_to_new_handle() {
        let dir = std::env::temp_dir().join(format!("sigil-delegate-xfer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = Arc::new(DelegateRegistry::new(Some(dir.clone())));
        reg.insert(DelegateState {
            id: "d00001".into(),
            name: "old".into(),
            status: DelegateStatus::Completed,
            started_ts: "t0".into(),
            finished_ts: Some("t1".into()),
            result: Some("r".into()),
            error: None,
            task: "task".into(),
            resume_from: None,
            steps: None,
            model: None,
            messages: Vec::new(),
            inbox: vec!["pending message".into()],
            tokens: None,
        })
        .await;
        reg.insert(DelegateState {
            id: "d00002".into(),
            name: "continuation".into(),
            status: DelegateStatus::Running,
            started_ts: "t2".into(),
            finished_ts: None,
            result: None,
            error: None,
            task: "task".into(),
            resume_from: Some("d00001".into()),
            steps: None,
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
        })
        .await;
        // The continuation inherits the parent's queued messages…
        reg.transfer_inbox("d00001", "d00002").await;
        let snap = reg.snapshot().await;
        let old = snap.iter().find(|s| s.id == "d00001").unwrap();
        let new = snap.iter().find(|s| s.id == "d00002").unwrap();
        assert!(old.inbox.is_empty(), "old record's inbox cleared");
        assert_eq!(new.inbox, vec!["pending message"]);
        // …and the transfer is persisted to both meta.jsons.
        let back_new: DelegateState = serde_json::from_str(
            &std::fs::read_to_string(dir.join("d00002").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back_new.inbox, vec!["pending message"]);
        let _ = std::fs::remove_dir_all(&dir);
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
            model: Some("model-y".into()),
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: reg.clone(),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
                model: None,
                messages: Vec::new(),
                inbox: Vec::new(),
                tokens: None,
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
            registry: ToolRegistry::with_tools(None, None, None, None, None, None),
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
                hooks: None,
                session_sink: None,
            },
            delegates: Arc::new(DelegateRegistry::default()),
            agents: Arc::new(subagent::AgentRegistry::new(None)),
            description: std::sync::OnceLock::new(),
            depth: 0,
            progress: None,
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
            model: None,
            messages: Vec::new(),
            inbox: Vec::new(),
            tokens: None,
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
                ToolRegistry::with_tools(None, None, None, None, None, None),
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
                    hooks: None,
                    session_sink: None,
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
        let mut reg = ToolRegistry::with_tools(None, None, None, None, None, None);
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
            hooks: None,
            session_sink: None,
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
