//! Harness tests: skill loading and prompt assembly.

use crate::harness::Harness;

#[test]
fn small_skills_are_inlined_into_the_prompt() {
    let dir = std::env::temp_dir().join(format!("sigil-harness-test-{}", std::process::id()));
    let skills = dir.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("tiny.md"), "> A tiny skill\n\njust a few bytes").unwrap();

    let h = Harness::load(Some(&dir)).unwrap();
    let prompt = h.full_system_prompt();

    // Index entry present...
    assert!(prompt.contains("**tiny** — A tiny skill"));
    // ...and the body is inlined.
    assert!(prompt.contains("just a few bytes"));
    assert!(prompt.contains("<skill name=\"tiny\">"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn large_skills_stay_as_index_only() {
    let dir = std::env::temp_dir().join(format!("sigil-harness-test-big-{}", std::process::id()));
    let skills = dir.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    // A skill larger than the 2 KiB inline threshold.
    let big = format!("> Big skill\n\n{}", "x".repeat(3 * 1024));
    std::fs::write(skills.join("big.md"), big).unwrap();

    let h = Harness::load(Some(&dir)).unwrap();
    let prompt = h.full_system_prompt();

    assert!(prompt.contains("**big** — Big skill"));
    // Body is NOT inlined (no <skill> block for it).
    assert!(!prompt.contains("<skill name=\"big\">"));

    std::fs::remove_dir_all(&dir).ok();
}
