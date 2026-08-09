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

/// List memory entries as numbered lines (1-based). Includes every
/// non-empty line; the model can then delete by index.
pub fn list(path: &PathBuf) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Ok("(memory is empty)".into());
    }
    let out: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:3}. {}", i + 1, l))
        .collect();
    Ok(format!("{} entries:\n{}", lines.len(), out.join("\n")))
}

/// Delete memory entries by 1-based index (from `list`) or by substring
/// (case-insensitive). Returns what was removed.
pub fn delete(
    path: &PathBuf,
    index: Option<usize>,
    substring: Option<&str>,
) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut removed: Vec<String> = Vec::new();
    match (index, substring) {
        (Some(i), _) if i >= 1 && i <= lines.len() => {
            removed.push(lines.remove(i - 1));
        }
        (Some(i), _) => {
            return Ok(format!(
                "index {i} out of range (memory has {} lines)",
                lines.len()
            ));
        }
        (None, Some(sub)) if !sub.trim().is_empty() => {
            let lower = sub.to_lowercase();
            let mut kept = Vec::with_capacity(lines.len());
            for l in lines {
                if l.to_lowercase().contains(&lower) {
                    removed.push(l);
                } else {
                    kept.push(l);
                }
            }
            lines = kept;
        }
        _ => return Ok("delete needs 'index' (from list) or 'substring'".into()),
    }
    let updated = lines.join("\n");
    std::fs::write(path, updated)?;
    if removed.is_empty() {
        Ok("nothing removed".into())
    } else {
        Ok(format!(
            "removed {} entr{}:\n{}",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" },
            removed.join("\n")
        ))
    }
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

    #[test]
    fn list_numbers_entries() {
        let dir = std::env::temp_dir().join(format!("sigil-mem-{}", std::process::id()));
        let path = dir.join("memory.md");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "- (2026-08-09) alpha\n- (2026-08-09) beta\n\n").unwrap();

        let out = list(&path).unwrap();
        assert!(out.contains("2 entries"));
        assert!(out.contains("  1. - (2026-08-09) alpha"));
        assert!(out.contains("  2. - (2026-08-09) beta"));

        // Empty file.
        std::fs::write(&path, "").unwrap();
        assert!(list(&path).unwrap().contains("empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_by_index_and_substring() {
        let dir = std::env::temp_dir().join(format!("sigil-mem-{}", std::process::id()));
        let path = dir.join("memory.md");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "- (2026-08-09) alpha\n- (2026-08-09) beta\n- (2026-08-09) gamma\n",
        )
        .unwrap();

        // By index: remove entry 2 (beta).
        let out = delete(&path, Some(2), None).unwrap();
        assert!(out.contains("beta"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("beta"));

        // Out of range index is reported cleanly.
        let out = delete(&path, Some(99), None).unwrap();
        assert!(out.contains("out of range"));

        // By substring: remove gamma.
        let out = delete(&path, None, Some("gamma")).unwrap();
        assert!(out.contains("gamma"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("gamma") && text.contains("alpha"));

        // Neither arg → guidance.
        let out = delete(&path, None, None).unwrap();
        assert!(out.contains("delete needs"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
