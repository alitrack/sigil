//! Safety unit tests: gate decisions, rings, circuit breaker.

use crate::safety::{CircuitBreaker, GateDecision, Ring, authorize_write_in, check, ring_of};
use serde_json::json;

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn bash(cmd: &str) -> GateDecision {
        check("bash", &json!({ "command": cmd }))
    }

    #[test]
    fn rejects_rm_rf() {
        assert_eq!(bash("rm -rf /"), GateDecision::Reject);
        assert_eq!(bash("rm -rf ~/.sigil"), GateDecision::Reject);
    }

    #[test]
    fn rejects_force_push_and_reset() {
        assert_eq!(bash("git push --force origin main"), GateDecision::Reject);
        assert_eq!(bash("git reset --hard HEAD~1"), GateDecision::Reject);
        assert_eq!(bash("git clean -fdx"), GateDecision::Reject);
    }

    #[test]
    fn rejects_bare_destructive_binaries() {
        assert_eq!(bash("rm /tmp/x"), GateDecision::Reject); // bare rm
        assert_eq!(bash("kill -9 1234"), GateDecision::Reject);
        assert_eq!(bash("sudo apt remove python"), GateDecision::Reject);
    }

    #[test]
    fn rejects_db_drop() {
        assert_eq!(bash("psql -c 'DROP TABLE users;'"), GateDecision::Reject);
        assert_eq!(bash("mysql -e 'drop database app'"), GateDecision::Reject);
    }

    #[test]
    fn allows_safe_commands() {
        assert_eq!(bash("ls -la"), GateDecision::Allow);
        assert_eq!(bash("cargo test"), GateDecision::Allow);
        assert_eq!(bash("git status"), GateDecision::Allow);
        assert_eq!(bash("git diff"), GateDecision::Allow);
        assert_eq!(bash("echo hello"), GateDecision::Allow);
    }

    #[test]
    fn allows_read_only_git() {
        assert_eq!(bash("git log --oneline -5"), GateDecision::Allow);
        assert_eq!(bash("git branch -a"), GateDecision::Allow);
    }

    #[test]
    fn non_bash_tools_pass() {
        assert_eq!(
            check("read_file", &json!({"path": "/etc/hosts"})),
            GateDecision::Allow
        );
        assert_eq!(
            check("write_file", &json!({"path": "x", "content": "y"})),
            GateDecision::Allow
        );
    }

    #[test]
    fn classifies_execution_rings() {
        assert_eq!(ring_of("read_file"), Ring::ReadOnly);
        assert_eq!(ring_of("git_status"), Ring::ReadOnly);
        assert_eq!(ring_of("create_lua_tool"), Ring::Harness);
        assert_eq!(ring_of("bash"), Ring::Workspace);
        assert_eq!(ring_of("write_file"), Ring::Workspace);
    }

    #[test]
    fn circuit_breaker_trips_after_three_failures() {
        let cb = CircuitBreaker::new();
        assert!(!cb.is_tripped("bash"));
        assert!(!cb.record_failure("bash"));
        assert!(!cb.record_failure("bash"));
        assert!(cb.record_failure("bash"));
        assert!(cb.is_tripped("bash"));
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new();
        cb.record_failure("bash");
        cb.record_failure("bash");
        cb.record_success("bash");
        assert!(!cb.is_tripped("bash"));
        cb.record_failure("bash");
        cb.record_failure("bash");
        cb.record_failure("bash");
        assert!(cb.is_tripped("bash"));
        cb.record_success("bash");
        assert!(!cb.is_tripped("bash"));
    }

    #[test]
    fn circuit_breaker_is_per_tool() {
        let cb = CircuitBreaker::new();
        cb.record_failure("bash");
        cb.record_failure("bash");
        cb.record_failure("bash");
        assert!(cb.is_tripped("bash"));
        assert!(!cb.is_tripped("read_file"));
    }
}

#[test]
fn gates_every_shell_command_and_git_global_options() {
    for command in [
        "true; rm /tmp/x",
        "echo ok && /bin/rm /tmp/x",
        "ls | rm /tmp/x",
        "echo ok\nrm /tmp/x",
        "git -C /tmp push origin main",
        "git --git-dir=/tmp/repo reset --hard",
        "env FLAG=1 git -c color.ui=false clean -fd",
        "command rm /tmp/x",
        "bash -c 'rm /tmp/x'",
        "echo $(rm /tmp/x)",
        "echo `rm /tmp/x`",
        "while rm /tmp/x; do break; done",
        "until rm /tmp/x; do break; done",
        "time rm /tmp/x",
        "builtin eval 'rm /tmp/x'",
        "git -C /tmp --no-pager push",
        "env -S 'rm /tmp/x'",
        "timeout 3 rm /tmp/x",
        "> /tmp/out rm /tmp/x",
        ">/tmp/out rm /tmp/x",
        "time -p rm /tmp/x",
        "env -S 'git -C /tmp' push",
        "2>/tmp/out rm /tmp/x",
        "tool=rm; $tool /tmp/x",
        "r{m,mdir} victim",
        "git -C /tmp {push,status}",
        "cat <<EOF\n$(rm /tmp/x)\nEOF",
    ] {
        assert_eq!(
            check("bash", &json!({"command": command})),
            GateDecision::Reject,
            "{command}"
        );
    }
    for command in [
        "cd /tmp && git -C /tmp status",
        "cargo test && cargo fmt -- --check",
        "git -c color.ui=false log --oneline | head -10",
        "echo 'rm /tmp/x'",
        "printf '%s' \"hello; world\"",
        "FLAG=1 cargo test",
        "echo $HOME",
        "command -v rm",
        "env printf '%s' '-S' 'rm /tmp/x'",
        "bash -c ''",
        "cat <<'EOF'\nrm /tmp/x\nEOF",
        "cat <<EOF\nrm /tmp/x\nEOF",
        "cat <<'EOF'\n$(rm /tmp/x)\nEOF",
        "cat <<EOF\nhello $(pwd)\nEOF",
        "cat <<< 'rm /tmp/x'",
        "echo $(pwd)",
        "printf '%s' ''",
        "cat <<-EOF\n\trm /tmp/x\n\tEOF",
        "cat <<'ONE' <<'TWO'\nrm /tmp/x\nONE\ngit push\nTWO",
    ] {
        assert_eq!(
            check("bash", &json!({"command": command})),
            GateDecision::Allow,
            "{command}"
        );
    }
}

#[test]
fn git_aliases_and_unknown_helpers_require_approval() {
    // Gate evaluation alone: never execute these alias bodies.
    for command in [
        "git -c 'alias.erase=!rm victim' erase",
        "git -calias.erase='!rm victim' erase",
        "git --config-env=alias.erase=ERASE erase",
        "git --config-env alias.erase=ERASE erase",
        "git -c include.path=/tmp/aliases erase",
        "git -C /tmp erase",
        "git erase",
        "git external-helper",
    ] {
        assert_eq!(
            check("bash", &json!({"command":command})),
            GateDecision::Reject,
            "{command}"
        );
    }
    for command in [
        "git -c core.foo=bar status",
        "git -c color.ui=false diff",
        "git --config-env core.foo=FOO status",
        "git -C /tmp --no-pager log",
        "git for-each-ref --format='%(refname)'",
        "git add src/safety/mod.rs",
        "git commit -m 'fix: approval gate'",
        "git --version",
    ] {
        assert_eq!(
            check("bash", &json!({"command":command})),
            GateDecision::Allow,
            "{command}"
        );
    }
}

// ---------------------------------------------------------------------------
// Write-scope policy: a model-directed write must stay inside an allowed root.
// Roots are passed explicitly (never via env) so these run safely in parallel.
// ---------------------------------------------------------------------------

fn root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sigil-write-scope-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn write_inside_allowed_root_is_accepted() {
    let base = root("inside");
    let roots = vec![base.clone()];
    let target = base.join("sub/file.txt");
    let ok = authorize_write_in(&target, &roots);
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn write_with_parent_escape_is_refused() {
    let base = root("escape");
    let roots = vec![base.clone()];
    let sneaky = base.join("sub/../../etc/passwd");
    let err = authorize_write_in(&sneaky, &roots).unwrap_err().to_string();
    assert!(err.contains("write refused"), "{err}");
}

#[test]
fn write_to_absolute_path_outside_roots_is_refused() {
    let base = root("absolute");
    let roots = vec![base];
    let err = authorize_write_in(std::path::Path::new("/etc/passwd"), &roots)
        .unwrap_err()
        .to_string();
    assert!(err.contains("write refused"), "{err}");
}

#[test]
fn write_roots_can_be_extended_explicitly() {
    let base = root("extend-a");
    let extra = root("extend-b");
    let target = extra.join("notes.md");
    assert!(authorize_write_in(&target, std::slice::from_ref(&base)).is_err());
    assert!(authorize_write_in(&target, &[base, extra]).is_ok());
}

#[test]
fn sibling_prefix_is_not_treated_as_inside() {
    // /tmp/x-evil must not pass just because /tmp/x is a root.
    let base = root("prefix");
    let sibling = base.with_file_name(format!(
        "{}-evil",
        base.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&sibling).unwrap();
    let err = authorize_write_in(&sibling.join("f.txt"), &[base])
        .unwrap_err()
        .to_string();
    assert!(err.contains("write refused"), "{err}");
}
