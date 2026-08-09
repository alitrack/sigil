//! Approval gate — the safety model.
//!
//! Phase 1: dangerous-command detection + non-interactive rejection.
//! The agent loop checks each tool call against this gate before
//! execution:
//!   - Safe (read-only) commands: run freely.
//!   - Mutating commands: run (write tools are the agent's job), but
//!     truly destructive commands are rejected in non-interactive
//!     (pipe) mode, where no human can approve them.
//!
//! Interactive approval (prompt-based, like Hermes' approval gate) lands
//! with the TUI in a later phase.

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

/// Classify a tool call and decide.
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
