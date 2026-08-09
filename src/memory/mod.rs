//! Persistent memory for the agent: a single markdown file
//! (`~/.sigil/memory.md`) the model itself maintains.
//!
//! Two tools expose it:
//!   remember — append a fact (with date), skip if it already exists
//!   recall   — show the current memory file (or grep it)
//!
//! Unlike a vector store, this is intentionally plain text: the model
//! reads it when it needs context and edits it when it learns
//! something. The harness prompt points at it ("before starting,
//! recall prior facts").

use std::path::PathBuf;

/// Default memory file location.
pub fn default_memory_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("sigil")
        .join("memory.md")
}

/// Append a fact to the memory file (idempotent by substring).
pub fn remember(path: &PathBuf, fact: &str) -> anyhow::Result<String> {
    let fact = fact.trim();
    if fact.is_empty() {
        return Ok("nothing to remember (empty fact)".into());
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(fact) {
        return Ok("already in memory — skipped".into());
    }
    let date = chrono_lite_today();
    let entry = format!("- ({date}) {fact}\n");
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)?;
    Ok(format!("remembered: {fact}"))
}

/// Show the memory file, optionally filtered by a search term.
pub fn recall(path: &PathBuf, filter: Option<&str>) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return Ok("(memory is empty)".into());
    }
    match filter {
        Some(term) if !term.trim().is_empty() => {
            let matches: Vec<&str> = text
                .lines()
                .filter(|l| l.to_lowercase().contains(&term.to_lowercase()))
                .collect();
            if matches.is_empty() {
                Ok(format!("no memory entries matching '{term}'"))
            } else {
                Ok(matches.join("\n"))
            }
        }
        _ => Ok(text),
    }
}

/// Today's date as YYYY-MM-DD without pulling in chrono.
fn chrono_lite_today() -> String {
    // std has no date formatting; use a tiny wall-clock via libc-free
    // approach: parse `date +%F`? No — keep it pure Rust with std.
    // We shell out once per remember call; acceptable for a CLI tool.
    std::process::Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_appends_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("sigil-mem-{}", std::process::id()));
        let path = dir.join("memory.md");
        let _ = std::fs::remove_dir_all(&dir);

        let out1 = remember(&path, "user prefers short answers").unwrap();
        assert!(out1.contains("remembered"));
        let out2 = remember(&path, "user prefers short answers").unwrap();
        assert!(out2.contains("already in memory"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("user prefers short answers"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recall_filters() {
        let dir = std::env::temp_dir().join(format!("sigil-mem-{}", std::process::id()));
        let path = dir.join("memory.md");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "- (2026-08-09) the sky is blue\n- (2026-08-09) cargo is slow\n",
        )
        .unwrap();

        assert_eq!(recall(&path, None).unwrap().lines().count(), 2);
        assert!(
            recall(&path, Some("cargo"))
                .unwrap()
                .contains("cargo is slow")
        );
        assert!(
            recall(&path, Some("nonexistent"))
                .unwrap()
                .contains("no memory")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
