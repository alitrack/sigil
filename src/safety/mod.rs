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
use std::path::{Component, Path, PathBuf};
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

// ---------------------------------------------------------------------------
// Freeze and write-scope — the two chokepoints every write surface shares.
// ---------------------------------------------------------------------------

/// True when this run is frozen (`SIGIL_FREEZE=1`): every write to the
/// evolution surface (memory, scripted tools, harness, hooks) must be refused
/// so a benchmark's held-out phase measures transfer rather than further
/// mutation. Single source of truth — every write-capable tool calls this, so
/// a new write surface cannot silently skip the freeze.
pub fn is_frozen() -> bool {
    std::env::var_os("SIGIL_FREEZE").is_some_and(|v| v == "1")
}

/// Roots a model-directed write may land in: the workspace, the OS temp dir
/// and sigil's own state directory. `SIGIL_WRITE_ROOTS` (split on the platform
/// path separator) extends the list for hosts that genuinely need it.
pub fn write_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    roots.push(std::env::temp_dir());
    // On macOS/BSD `temp_dir()` is $TMPDIR (/var/folders/...) while `/tmp` is
    // a separate, legitimate scratch location that canonicalizes to
    // /private/tmp. Found by live testing: a write to /tmp was refused.
    #[cfg(unix)]
    roots.push(PathBuf::from("/tmp"));
    if let Some(config) = dirs::config_dir() {
        roots.push(config.join("sigil"));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".sigil"));
    }
    if let Some(extra) = std::env::var_os("SIGIL_WRITE_ROOTS") {
        for piece in std::env::split_paths(&extra) {
            if !piece.as_os_str().is_empty() {
                roots.push(piece);
            }
        }
    }
    roots
}

/// Canonicalize when the path exists; otherwise canonicalize the nearest
/// existing ancestor and re-attach the remaining components lexically. Keeps
/// symlinked roots (`/var` -> `/private/var`) comparable without requiring the
/// target file to exist yet.
fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    while let Some(name) = cursor.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            break;
        };
        if let Ok(real) = std::fs::canonicalize(parent) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        cursor = parent;
    }
    lexical_normalize(path)
}

/// Resolve `.` and pop on `..` without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Authorize a model-directed write against an explicit set of roots.
///
/// Split out from [`authorize_write`] so tests can exercise the policy without
/// mutating process-global state (env vars / cwd), which parallel tests cannot
/// do safely.
pub fn authorize_write_in(path: &Path, roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let resolved = crate::agent::provider::resolve_path(path);
    let target = canonical_or_lexical(&resolved);
    for root in roots {
        let root = canonical_or_lexical(root);
        if target == root || target.starts_with(&root) {
            return Ok(resolved);
        }
    }
    let allowed = roots
        .iter()
        .map(|r| canonical_or_lexical(r).display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "write refused: {} is outside the writable roots ({allowed}). \
         Writes are limited to the workspace, the temp dir and sigil's state \
         dir; set SIGIL_WRITE_ROOTS to extend.",
        target.display()
    )
}

/// Authorize a model-directed write path (workspace / temp / sigil state).
pub fn authorize_write(path: &Path) -> anyhow::Result<PathBuf> {
    authorize_write_in(path, &write_roots())
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
    if shell_requires_approval(cmd, 0) {
        GateDecision::Reject
    } else {
        GateDecision::Allow
    }
}

/// Inspect each simple command, preserving quoted arguments and inspecting
/// command substitutions separately. This is an approval heuristic, not a
/// shell sandbox; opaque executable expansions require human review.
fn shell_requires_approval(source: &str, depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    let chars: Vec<char> = source.chars().collect();
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut heredocs = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                word.push(c);
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            i += 1;
            if let Some(&next) = chars.get(i) {
                if next != '\n' {
                    word.push(next);
                    started = true;
                }
                i += 1;
            }
            continue;
        }
        if c == '`'
            || ((c == '$' || (quote.is_none() && matches!(c, '<' | '>')))
                && chars.get(i + 1) == Some(&'('))
        {
            let Some((end, inner)) = substitution(&chars, i) else {
                return true;
            };
            if shell_requires_approval(&inner, depth + 1) {
                return true;
            }
            word.push('$'); // opaque expansion if used as the executable
            started = true;
            i = end + 1;
            continue;
        }
        if c == '"' {
            started = true;
            if quote == Some('"') {
                quote = None;
            } else {
                quote = Some('"');
            }
        } else if quote.is_some() {
            word.push(c);
        } else if c == '\'' {
            started = true;
            quote = Some(c);
        } else if c == '#' && !started {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if matches!(c, '<' | '>') {
            // File descriptor prefixes belong to the redirect, not the
            // executable. Keep redirect operands out of command lookup.
            let mut redirect = if !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
                std::mem::take(&mut word)
            } else {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                String::new()
            };
            started = false;
            redirect.push(c);
            i += 1;
            while chars.get(i) == Some(&c) {
                redirect.push(c);
                i += 1;
            }
            if chars.get(i) == Some(&'&') {
                redirect.push('&');
                i += 1;
            }
            if redirect.trim_start_matches(|c: char| c.is_ascii_digit()) == "<<" {
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
                    i += 1;
                }
                let mut delimiter = String::new();
                let mut quoted = false;
                let mut delimiter_quote = None;
                while let Some(&t) = chars.get(i) {
                    if delimiter_quote == Some(t) {
                        delimiter_quote = None;
                        quoted = true;
                    } else if delimiter_quote.is_some() {
                        delimiter.push(t);
                    } else if matches!(t, '\'' | '"') {
                        delimiter_quote = Some(t);
                        quoted = true;
                    } else if t == '\\' {
                        quoted = true;
                        i += 1;
                        if let Some(&t) = chars.get(i) {
                            delimiter.push(t);
                        }
                    } else if t.is_whitespace() || matches!(t, ';' | '&' | '|') {
                        break;
                    } else {
                        delimiter.push(t);
                    }
                    i += 1;
                }
                if delimiter.is_empty() || delimiter_quote.is_some() {
                    return true;
                }
                words.push(redirect);
                words.push(delimiter.clone());
                heredocs.push((delimiter, quoted, strip_tabs));
            } else {
                words.push(redirect);
            }
            continue;
        } else if matches!(c, ';' | '&' | '|' | '(' | ')' | '\n') {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
            if !words.is_empty() {
                commands.push(std::mem::take(&mut words));
            }
            if c == '\n' && !heredocs.is_empty() {
                i += 1;
                for (delimiter, quoted, strip_tabs) in heredocs.drain(..) {
                    let body_start = i;
                    loop {
                        if i >= chars.len() {
                            return true;
                        }
                        let line_start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[line_start..i].iter().collect();
                        if (if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            &line
                        }) == delimiter
                        {
                            if !quoted
                                && expansions_require_approval(
                                    &chars[body_start..line_start],
                                    depth,
                                )
                            {
                                return true;
                            }
                            if i < chars.len() {
                                i += 1;
                            }
                            break;
                        }
                        if i < chars.len() {
                            i += 1;
                        }
                    }
                }
                continue;
            }
        } else if c.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            started = true;
            word.push(c);
        }
        i += 1;
    }
    if quote.is_some() || !heredocs.is_empty() {
        return true;
    }
    if started {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    commands
        .iter()
        .any(|words| command_requires_approval(words, depth))
}

/// Return the source inside a command/process substitution and its closing
/// delimiter. Its commands are checked recursively rather than mistaken for
/// arguments of the outer command.
fn substitution(chars: &[char], at: usize) -> Option<(usize, String)> {
    let backtick = chars[at] == '`';
    let start = at + if backtick { 1 } else { 2 };
    let mut i = start;
    let mut nesting = 1;
    let mut quote = None;
    while i < chars.len() {
        let t = chars[i];
        if t == '\\' {
            i += 2;
            continue;
        }
        if !backtick && matches!(t, '\'' | '"') {
            if quote == Some(t) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(t);
            }
        } else if quote.is_none() {
            if backtick && t == '`' {
                return Some((i, chars[start..i].iter().collect()));
            }
            if !backtick && t == '(' {
                nesting += 1;
            }
            if !backtick && t == ')' {
                nesting -= 1;
                if nesting == 0 {
                    return Some((i, chars[start..i].iter().collect()));
                }
            }
        }
        i += 1;
    }
    None
}

fn expansions_require_approval(chars: &[char], depth: usize) -> bool {
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' || (chars[i] == '$' && chars.get(i + 1) == Some(&'(')) {
            let Some((end, inner)) = substitution(chars, i) else {
                return true;
            };
            if shell_requires_approval(&inner, depth + 1) {
                return true;
            }
            i = end;
        }
        i += 1;
    }
    false
}

// Only recognized built-in Git commands auto-run. An unknown name can resolve
// to a shell alias from local/global configuration, inline -c/--config-env, or
// an external git-* helper. Do not execute Git to resolve that ambiguity during
// approval; new/custom commands require review. Ordinary -c overrides on a
// recognized command retain their existing behavior.
const GIT_BUILTINS: &str = "
    add am annotate apply archive bisect blame branch bugreport bundle cat-file
    check-attr check-ignore check-mailmap check-ref-format checkout checkout-index
    cherry cherry-pick clean clone column commit commit-graph commit-tree config
    count-objects credential credential-cache credential-store describe diff
    diff-files diff-index diff-tree difftool fast-export fast-import fetch
    fetch-pack fmt-merge-msg for-each-ref for-each-repo format-patch fsck
    fsck-objects gc get-tar-commit-id grep hash-object help index-pack init init-db
    interpret-trailers log ls-files ls-remote ls-tree mailinfo mailsplit maintenance
    merge merge-base merge-file merge-index merge-ours merge-recursive
    merge-subtree merge-tree mktag mktree multi-pack-index mv name-rev notes
    pack-objects pack-redundant pack-refs patch-id prune prune-packed pull push
    range-diff read-tree rebase receive-pack reflog remote repack replace rerere
    reset restore rev-list rev-parse revert rm send-pack shortlog show show-branch
    show-index show-ref sparse-checkout stage stash status stripspace switch
    symbolic-ref tag unpack-file unpack-objects update-index update-ref
    update-server-info upload-archive upload-pack var verify-commit verify-pack
    verify-tag version whatchanged worktree write-tree
";

fn command_requires_approval(words: &[String], depth: usize) -> bool {
    let mut i = 0;
    while i < words.len() {
        let token = words[i].as_str();
        if matches!(
            token,
            "if" | "then" | "elif" | "else" | "do" | "while" | "until" | "!" | "{" | "}"
        ) || token.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name.chars().enumerate().all(|(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
        }) {
            i += 1;
            continue;
        }
        if token
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .starts_with(['<', '>'])
        {
            i += 2;
            continue;
        }
        let executable = token.rsplit('/').next().unwrap_or(token);
        if executable == "command"
            && words
                .get(i + 1)
                .is_some_and(|flag| matches!(flag.as_str(), "-v" | "-V"))
        {
            return false; // inspect a command name without executing it
        }
        if executable == "env" {
            let mut at = i + 1;
            while at < words.len() {
                let flag = words[at].as_str();
                if matches!(flag, "-S" | "--split-string") {
                    return words.get(at + 1).is_none_or(|script| {
                        shell_requires_approval(
                            &format!("{} {}", script, words[at + 2..].join(" ")),
                            depth + 1,
                        )
                    });
                }
                if let Some(script) = flag.strip_prefix("--split-string=") {
                    return shell_requires_approval(
                        &format!("{} {}", script, words[at + 1..].join(" ")),
                        depth + 1,
                    );
                }
                if !flag.starts_with('-') && !flag.contains('=') {
                    break;
                }
                let takes_value = matches!(flag, "-u" | "--unset" | "-C" | "--chdir");
                at += 1 + usize::from(takes_value);
            }
        }
        if executable == "time" {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let takes_value =
                    matches!(words[i].as_str(), "-o" | "--output" | "-f" | "--format");
                i += 1 + usize::from(takes_value);
            }
            continue;
        }
        if executable == "timeout" {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let takes_value =
                    matches!(words[i].as_str(), "-s" | "--signal" | "-k" | "--kill-after");
                i += 1 + usize::from(takes_value);
            }
            i += 1; // duration
            continue;
        }
        if matches!(executable, "env" | "command" | "exec" | "nohup" | "builtin") {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let takes_value = matches!(words[i].as_str(), "-u" | "--unset" | "-C" | "--chdir");
                i += 1 + usize::from(takes_value);
            }
            continue;
        }
        if executable.contains(['$', '*', '?', '{', '}']) {
            return true;
        }
        if matches!(
            executable,
            "rm" | "kill" | "pkill" | "sudo" | "rmdir" | "mkfs" | "shutdown" | "reboot"
        ) {
            return true;
        }
        if executable == "git" {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let takes_value = matches!(
                    words[i].as_str(),
                    "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
                );
                i += 1 + usize::from(takes_value);
            }
            return words.get(i).is_some_and(|sub| {
                !GIT_BUILTINS
                    .split_ascii_whitespace()
                    .any(|builtin| builtin == sub)
                    || matches!(
                        sub.as_str(),
                        "push" | "reset" | "clean" | "checkout" | "revert" | "merge" | "rebase"
                    )
            });
        }
        if executable == "eval" {
            return shell_requires_approval(&words[i + 1..].join(" "), depth + 1);
        }
        if matches!(executable, "bash" | "sh" | "zsh" | "dash" | "ksh") {
            for (at, flag) in words.iter().enumerate().skip(i + 1) {
                if flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c') {
                    return words
                        .get(at + 1)
                        .is_none_or(|script| shell_requires_approval(script, depth + 1));
                }
            }
        }
        return false;
    }
    false
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
