//! Safety model — three layers, borrowed from forge's safety module:
//!
//! 1. **Approval gate** (activegate): destructive-command detection +
//!    non-interactive rejection. Every tool call passes through here.
//! 2. **Circuit breaker** (circuit_breaker): a tool that fails repeatedly
//!    in a row gets tripped — further calls are short-circuited with a
//!    clear message instead of burning more turns on a broken tool.
//! 3. **Execution ring** (execution_ring): tools are classified by how
//!    much they can touch. Read-only tools (read_file, git_status, ...)
//!    run freely; mutating tools (write_file, bash) run but are gated;
//!    ring 3 (escape) tools like create_lua_tool run but their output
//!    is flagged to the model as "harness mutation".

use std::collections::HashMap;
use std::sync::Mutex;

use crate::tools::ToolOutput;

#[cfg(test)]
mod tests;

/// Decision for a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Execute normally.
    Allow,
    /// Reject with a message back to the model.
    Reject,
}

/// Execution ring: how much a tool can touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    /// Read-only inspection — always allowed (read_file, git_status, ...).
    ReadOnly,
    /// Mutates the workspace but is the agent's job (write_file, bash).
    Workspace,
    /// Mutates the harness itself (create_lua_tool) — allowed, but the
    /// model is told the mutation happened.
    Harness,
}

/// Classify a tool name into an execution ring.
pub fn ring_of(name: &str) -> Ring {
    match name {
        "read_file" | "git_status" | "git_diff" | "git_log" => Ring::ReadOnly,
        "create_lua_tool" => Ring::Harness,
        _ => Ring::Workspace,
    }
}

/// Classify a tool call and decide (approval gate).
pub fn check(call_name: &str, arguments: &serde_json::Value) -> GateDecision {
    match call_name {
        "bash" => check_bash(arguments),
        _ => GateDecision::Allow,
    }
}

/// Bash commands that must never auto-run without a human present.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r",
    "rmdir",
    "mkfs",
    "dd if=",
    "shutdown",
    "reboot",
    "> /dev/sd",
    ":(){ :|:& };:",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "git clean -f",
    "git checkout .",
    "git checkout -- .",
    "drop database",
    "drop table",
    "DROP DATABASE",
    "DROP TABLE",
    "kill -9",
    "pkill",
];

/// Evaluate a bash tool call.
fn check_bash(arguments: &serde_json::Value) -> GateDecision {
    let Some(cmd) = arguments.get("command").and_then(serde_json::Value::as_str) else {
        return GateDecision::Allow; // malformed; tool itself will error
    };
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return GateDecision::Allow;
    }
    for pat in DESTRUCTIVE_PATTERNS {
        if cmd.contains(pat) {
            return GateDecision::Reject;
        }
    }
    // Bare `rm`/`kill`/`sudo` are suspicious and rejected. `git` is
    // allowed for read-only subcommands (status/diff/log/branch/...);
    // destructive git subcommands were already caught by the pattern list.
    let first = cmd.split_whitespace().next().unwrap_or("");
    if matches!(first, "rm" | "kill" | "pkill" | "sudo") {
        return GateDecision::Reject;
    }
    if first == "git" {
        let sub = cmd.split_whitespace().nth(1).unwrap_or("");
        if matches!(
            sub,
            "push" | "reset" | "clean" | "checkout" | "revert" | "merge" | "rebase"
        ) {
            return GateDecision::Reject;
        }
    }
    GateDecision::Allow
}

/// Render a rejection message for the model.
pub fn reject_message(call_name: &str, arguments: &serde_json::Value) -> String {
    let cmd = arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    format!(
        "APPROVAL DENIED: the command `{cmd}` (tool `{call_name}`) was rejected by the \
         sigil approval gate. sigil runs non-interactively and cannot ask for \
         confirmation. Refactor the task to avoid destructive operations, or \
         run the command yourself outside sigil."
    )
}

/// Circuit breaker: trip a tool after N consecutive failures.
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    failures: Mutex<HashMap<String, u32>>,
}

impl CircuitBreaker {
    pub const TRIP_AFTER: u32 = 3;

    pub fn new() -> Self {
        Self::default()
    }

    /// Is this tool currently tripped (open)?
    pub fn is_tripped(&self, tool: &str) -> bool {
        self.failures
            .lock()
            .map(|f| f.get(tool).copied().unwrap_or(0) >= Self::TRIP_AFTER)
            .unwrap_or(false)
    }

    /// Record a tool failure; returns true when it just tripped.
    pub fn record_failure(&self, tool: &str) -> bool {
        let mut f = self.failures.lock().unwrap();
        let n = f.entry(tool.to_string()).or_insert(0);
        *n += 1;
        *n >= Self::TRIP_AFTER
    }

    /// Record a success (reset the counter).
    pub fn record_success(&self, tool: &str) {
        if let Ok(mut f) = self.failures.lock() {
            f.remove(tool);
        }
    }

    /// Tripped message shown to the model.
    pub fn tripped_message(tool: &str) -> String {
        format!(
            "CIRCUIT BREAKER: tool `{tool}` failed {} times in a row and was \
             automatically tripped. Fix the underlying issue (or the tool's \
             inputs) before calling it again; do not retry blindly.",
            Self::TRIP_AFTER
        )
    }
}

/// Gate-aware tool output wrapper.
#[allow(dead_code)]
pub fn to_output(
    decision: GateDecision,
    call_name: &str,
    arguments: &serde_json::Value,
) -> Option<ToolOutput> {
    match decision {
        GateDecision::Allow => None,
        GateDecision::Reject => Some(ToolOutput {
            text: reject_message(call_name, arguments),
        }),
    }
}
