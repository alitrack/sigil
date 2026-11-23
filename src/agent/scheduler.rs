//! Scheduler-level guards for the agent loop.
//!
//! Two failure modes that the benchmark runs made explicit, both of which cost
//! nothing to fix at the scheduler level and everything to fix with a better
//! model:
//!
//! 1. **Idle spinning** — a run burns its whole step budget producing nothing,
//!    when a single tool-less call up front would have scored. The floor is
//!    cheap: take one draft *before* the loop and fall back to it if the loop
//!    ends without a usable answer.
//! 2. **No progress** — the same tool round repeats with the same result, and
//!    the run keeps paying for it. Detect the repetition, tell the model once,
//!    then stop and ask for a wrap-up.
//!
//! Both are opt-in through the environment rather than `config.toml` / CLI: a
//! scheduler experiment should not permanently enlarge the configuration
//! surface (see `review-multidomain-263bd58.md` §3).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// Test-only override, thread-local on purpose: the real switches are
// process-global env vars, and two tests toggling them concurrently would
// poison each other (precisely the race this project has hit before with
// shared state). `#[tokio::test]` runs each test on its own thread.
#[cfg(test)]
thread_local! {
    static TEST_LIMITS: std::cell::RefCell<(Option<bool>, Option<u32>)> =
        const { std::cell::RefCell::new((None, None)) };
}

/// Set the scheduler limits for the current test thread.
#[cfg(test)]
pub fn set_test_limits(first_frame: Option<bool>, no_progress: Option<u32>) {
    TEST_LIMITS.with(|c| *c.borrow_mut() = (first_frame, no_progress));
}

/// Take a one-shot draft before entering the loop (`SIGIL_FIRST_FRAME=1`).
pub fn first_frame_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_LIMITS.with(|c| c.borrow().0) {
        return v;
    }
    std::env::var_os("SIGIL_FIRST_FRAME").is_some_and(|v| v == "1")
}

/// Stop after this many consecutive identical tool rounds (`SIGIL_NO_PROGRESS`,
/// 0 = off). Two is a sensible value: one repeat is often a legitimate retry.
pub fn no_progress_limit() -> u32 {
    #[cfg(test)]
    if let Some(v) = TEST_LIMITS.with(|c| c.borrow().1) {
        return v;
    }
    std::env::var_os("SIGIL_NO_PROGRESS")
        .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<u32>().ok()))
        .unwrap_or(0)
}

/// Stable signature of one executed tool round: the ordered `(name, ok,
/// output)` triples. Two rounds with the same signature achieved the same
/// thing, whatever the model thinks it is doing.
pub fn progress_signature(receipts: &[(String, bool, String)]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for (name, ok, output) in receipts {
        name.hash(&mut hasher);
        ok.hash(&mut hasher);
        output.hash(&mut hasher);
    }
    hasher.finish()
}

/// The nudge injected when a round repeats: state the fact, ask for a change of
/// approach, do not scold.
pub fn no_progress_nudge(repeats: u32) -> String {
    format!(
        "SCHEDULER: the last {repeats} tool round(s) produced results identical to \
         the round before. Repeating them will not change the outcome. Either \
         change approach (different tool, different arguments, or inspect the \
         failure you are ignoring), or stop and state your best answer with what \
         you know. If the next round produces the same result again, this run will \
         be wrapped up."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(name: &str, ok: bool, out: &str) -> (String, bool, String) {
        (name.to_string(), ok, out.to_string())
    }

    #[test]
    fn identical_rounds_hash_identically() {
        let a = vec![receipt("bash", true, "ok"), receipt("read_file", true, "x")];
        let b = vec![receipt("bash", true, "ok"), receipt("read_file", true, "x")];
        assert_eq!(progress_signature(&a), progress_signature(&b));
    }

    #[test]
    fn any_difference_changes_the_signature() {
        let base = vec![receipt("bash", true, "ok")];
        for other in [
            vec![receipt("bash", false, "ok")],
            vec![receipt("bash", true, "ok ")],
            vec![receipt("read_file", true, "ok")],
            vec![receipt("bash", true, "ok"), receipt("bash", true, "ok")],
        ] {
            assert_ne!(progress_signature(&base), progress_signature(&other));
        }
    }

    #[test]
    fn order_matters() {
        let a = vec![receipt("a", true, "1"), receipt("b", true, "2")];
        let b = vec![receipt("b", true, "2"), receipt("a", true, "1")];
        assert_ne!(progress_signature(&a), progress_signature(&b));
    }

    #[test]
    fn nudge_names_the_repeat_count_and_the_way_out() {
        let text = no_progress_nudge(2);
        assert!(text.contains('2'));
        assert!(text.contains("change approach"));
        assert!(text.contains("wrapped up"));
    }
}
