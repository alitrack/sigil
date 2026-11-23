//! The single tool-execution chokepoint.
//!
//! Every tool call — model-initiated today, and any future execution path —
//! must go through [`ToolGate`], which owns the whole policy:
//!
//! ```text
//! gate()    approval gate → interactive approver → on_pre_tool veto → breaker
//! run()     execute → execution-ring annotation → breaker bookkeeping
//! observe() session ToolResult event → on_tool_result rewrite
//! ```
//!
//! This is structural, not cosmetic. EDG (removed in `174f3fa`) was a second
//! execution path that re-implemented "run a tool" and therefore silently
//! skipped the hooks, the circuit breaker and the session sink — five P0
//! findings that survived a review for three weeks. A second path cannot drift
//! out of the safety model if running a tool *means* calling this type.
//!
//! Split into three phases because the middle one is concurrent: gating and
//! bookkeeping are cheap and serial, the tool runs are independent. Callers
//! that do not need the fan-out use [`ToolGate::execute`].

use anyhow::Result;

use crate::llm::provider::ToolCall;
use crate::safety;
use crate::tools::ToolRegistry;

use super::{AgentConfig, Approver};

/// Outcome of the serial gate phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Cleared to run.
    Run,
    /// Refused before execution; the string is what the model sees instead of
    /// a tool result.
    Blocked(String),
}

/// Owns the tool-execution policy for one turn.
pub struct ToolGate<'a> {
    registry: &'a ToolRegistry,
    breaker: &'a safety::CircuitBreaker,
    /// `None` for protocol paths (MCP `tools/call`) that have no agent
    /// config: hooks and the session sink then simply do not fire.
    config: Option<&'a AgentConfig>,
    approver: Option<&'a Approver>,
}

impl<'a> ToolGate<'a> {
    pub fn new(
        registry: &'a ToolRegistry,
        breaker: &'a safety::CircuitBreaker,
        config: Option<&'a AgentConfig>,
        approver: Option<&'a Approver>,
    ) -> Self {
        Self {
            registry,
            breaker,
            config,
            approver,
        }
    }

    /// Audit event for a gate-affected call (audit-only, never replayed into
    /// model context).
    fn emit_approval(&self, call: &ToolCall, decision: &str, reason: Option<String>) {
        if let Some(sink) = self.config.and_then(|c| c.session_sink.as_ref()) {
            sink(crate::session::Event::Approval {
                ts: None,
                name: call.name.clone(),
                args: call.arguments.to_string(),
                decision: decision.into(),
                reason,
            });
        }
    }

    /// Serial phase: decide whether this call may run.
    ///
    /// Order matters and is part of the contract: the approval gate rejects
    /// destructive `bash` in non-interactive mode; the approver may override;
    /// an `on_pre_tool` hook may veto; a tripped breaker short-circuits.
    pub async fn gate(&self, call: &ToolCall) -> Result<GateVerdict> {
        let gate = safety::check(&call.name, &call.arguments);
        let mut allowed = matches!(gate, safety::GateDecision::Allow);
        if !allowed
            && let Some(approve) = self.approver
            && approve(&call.name, &call.arguments)
        {
            allowed = true;
            eprintln!("⚠ approved by user: {} {}", call.name, call.arguments);
            self.emit_approval(call, "approved", None);
        }
        if !allowed {
            let decision = if self.approver.is_some() {
                "rejected"
            } else {
                "auto_rejected"
            };
            self.emit_approval(call, decision, None);
            return Ok(GateVerdict::Blocked(safety::reject_message(
                &call.name,
                &call.arguments,
            )));
        }
        // P0 hook (serial decision): an `on_pre_tool` listener can veto the
        // call before it runs. The first non-null verdict wins; a truthy
        // verdict (boolean true, or a non-empty string used as a
        // human-readable reason) blocks the call. A nil / null verdict is a
        // no-op and the chain continues.
        if let Some(hooks) = self.config.and_then(|c| c.hooks.as_ref())
            && let crate::hooks::DispatchOutput::First(Some(v)) = hooks
                .dispatch(
                    "on_pre_tool",
                    crate::hooks::HookMode::Serial,
                    serde_json::json!({
                        "name": call.name,
                        "args": call.arguments,
                    }),
                )
                .await?
        {
            let veto_reason = match &v {
                serde_json::Value::Bool(true) => Some("vetoed by on_pre_tool hook".to_string()),
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            if let Some(reason) = veto_reason {
                self.emit_approval(call, "hook_vetoed", Some(reason.clone()));
                return Ok(GateVerdict::Blocked(format!(
                    "blocked by on_pre_tool hook: {reason}"
                )));
            }
        }
        if self.breaker.is_tripped(&call.name) {
            return Ok(GateVerdict::Blocked(
                safety::CircuitBreaker::tripped_message(&call.name),
            ));
        }
        Ok(GateVerdict::Run)
    }

    /// Concurrent-safe phase: run the tool, annotate the execution ring, and
    /// record the outcome for the circuit breaker.
    ///
    /// Never fails: tool errors become model-visible output, matching
    /// `ToolRegistry::execute`'s contract.
    pub async fn run(&self, call: &ToolCall) -> String {
        let result = match self.registry.execute(call).await {
            Ok(r) => r,
            Err(e) => format!("error: {e}"),
        };
        // Execution ring: flag harness mutations so the model knows its
        // toolset changed.
        let result = match safety::ring_of(&call.name) {
            safety::Ring::Harness => format!("[harness mutation] {result}"),
            _ => result,
        };
        if result.starts_with("error:") {
            self.breaker.record_failure(&call.name);
        } else {
            self.breaker.record_success(&call.name);
        }
        result
    }

    /// Same as [`ToolGate::run`] but keeps the structured execution status,
    /// which protocol clients need (`isError` on an MCP response must not be
    /// inferred from text that legitimately starts with `error:`).
    pub async fn run_detailed(&self, call: &ToolCall) -> crate::tools::ToolExecution {
        let mut execution = match self.registry.execute_detailed(call).await {
            Ok(e) => e,
            Err(e) => crate::tools::ToolExecution {
                text: format!("error: {e}"),
                is_error: true,
            },
        };
        if matches!(safety::ring_of(&call.name), safety::Ring::Harness) {
            execution.text = format!("[harness mutation] {}", execution.text);
        }
        if execution.is_error {
            self.breaker.record_failure(&call.name);
        } else {
            self.breaker.record_success(&call.name);
        }
        execution
    }

    /// Serial phase: log the observed outcome, then let `on_tool_result`
    /// rewrite what the model sees. Returns the final model-visible text.
    pub async fn observe(&self, call: &ToolCall, executed: bool, result: String) -> Result<String> {
        if let Some(sink) = self.config.and_then(|c| c.session_sink.as_ref()) {
            let ok = executed && !result.starts_with("error:");
            // Cordis P3: annotate whether this call registered a
            // runtime-revertible effect so the session can be rolled back
            // frame by frame. Only a tool that opted into the EffectTracker
            // reports Some(true); everything else is None (not assessed).
            let revertible = self.registry.revertible_of(&call.name);
            sink(crate::session::Event::ToolResult {
                ts: None,
                id: call.id.clone(),
                name: call.name.clone(),
                ok,
                output: result.clone(),
                revertible,
            });
        }
        let msg = match self.config.and_then(|c| c.hooks.as_ref()) {
            Some(hooks) => hooks
                .run(
                    "on_tool_result",
                    serde_json::json!({
                        "name": call.name,
                        "args": call.arguments,
                        "result": result,
                    }),
                )
                .await?
                .unwrap_or(result),
            None => result,
        };
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentConfig;
    use crate::tools::{Tool, ToolOutput};
    use serde_json::json;

    struct FixedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for FixedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "fixed-output tool for gate tests"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput {
                text: "fine".into(),
            })
        }
    }

    struct FailingTool;

    #[async_trait::async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "failing_tool"
        }
        fn description(&self) -> &str {
            "always fails — drives the circuit breaker"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<ToolOutput> {
            anyhow::bail!("nope")
        }
    }

    fn config() -> AgentConfig {
        crate::agent::parallel_tests::test_config()
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("id_{name}"),
            name: name.into(),
            arguments: args,
        }
    }

    /// The three phases a caller runs, in order — the contract under test.
    async fn run_one(gate: &ToolGate<'_>, call: &ToolCall) -> String {
        let (executed, result) = match gate.gate(call).await.unwrap() {
            GateVerdict::Run => (true, gate.run(call).await),
            GateVerdict::Blocked(reason) => (false, reason),
        };
        gate.observe(call, executed, result).await.unwrap()
    }

    fn registry(names: &[&'static str]) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        for name in names {
            reg.register(std::sync::Arc::new(FixedTool(name)));
        }
        reg
    }

    #[tokio::test]
    async fn gate_blocks_destructive_bash_without_an_approver() {
        let reg = registry(&[]);
        let breaker = safety::CircuitBreaker::new();
        let cfg = config();
        let gate = ToolGate::new(&reg, &breaker, Some(&cfg), None);
        let verdict = gate
            .gate(&call("bash", json!({"command": "rm -rf /"})))
            .await
            .unwrap();
        assert!(matches!(verdict, GateVerdict::Blocked(_)), "{verdict:?}");
    }

    #[tokio::test]
    async fn execute_runs_gate_then_tool_then_observation() {
        let reg = registry(&["fixed_tool"]);
        let breaker = safety::CircuitBreaker::new();
        let cfg = config();
        let gate = ToolGate::new(&reg, &breaker, Some(&cfg), None);
        let out = run_one(&gate, &call("fixed_tool", json!({}))).await;
        assert_eq!(out, "fine");
    }

    #[tokio::test]
    async fn execute_returns_the_block_reason_instead_of_running() {
        let reg = registry(&[]);
        let breaker = safety::CircuitBreaker::new();
        let cfg = config();
        let gate = ToolGate::new(&reg, &breaker, Some(&cfg), None);
        let out = run_one(&gate, &call("bash", json!({"command": "git reset --hard"}))).await;
        assert!(
            out.contains("rejected") || out.contains("REJECTED"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn repeated_failures_trip_the_breaker_and_gate_short_circuits() {
        let mut reg = ToolRegistry::new();
        reg.register(std::sync::Arc::new(FailingTool));
        let breaker = safety::CircuitBreaker::new();
        let cfg = config();
        let gate = ToolGate::new(&reg, &breaker, Some(&cfg), None);
        for _ in 0..3 {
            let out = run_one(&gate, &call("failing_tool", json!({}))).await;
            assert!(out.starts_with("error:"), "{out}");
        }
        // Fourth call must not reach the tool at all.
        let verdict = gate.gate(&call("failing_tool", json!({}))).await.unwrap();
        assert!(matches!(verdict, GateVerdict::Blocked(_)), "{verdict:?}");
    }
}
