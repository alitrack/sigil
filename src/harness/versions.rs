//! Version library for model-writable assets (system prompt, skills,
//! scripted Lua tools).
//!
//! Every write performed by the evolution tools is snapshotted under
//! `<harness_dir>/versions/<kind>/<name>.<timestamp>.ext` before it is
//! applied, so a bad model edit can be listed and rolled back. This is
//! the safety net of the self-evolution loop: growth without loss.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Timestamp-based version id with nanosecond precision so rapid
/// successive writes to the same asset never collide.
pub fn now_stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
}

fn versions_root(harness_dir: &Path) -> PathBuf {
    harness_dir.join("versions")
}

fn kind_dir(harness_dir: &Path, kind: &str) -> PathBuf {
    versions_root(harness_dir).join(kind)
}

/// Snapshot the current content of an asset before it is overwritten.
///
/// `kind` is one of "prompt", "skills", "tools"; `name` is the asset's
/// base name (no extension). If the asset doesn't exist yet (first
/// write), nothing is backed up — there is nothing to lose.
pub fn backup(harness_dir: &Path, kind: &str, name: &str, content: &str) {
    let dir = kind_dir(harness_dir, kind);
    let Ok(_) = std::fs::create_dir_all(&dir) else {
        return;
    };
    let ext = match kind {
        "tools" => "lua",
        _ => "md",
    };
    let path = dir.join(format!("{name}.{}.{ext}", now_stamp()));
    let _ = std::fs::write(path, content);
}

/// List available versions of an asset, newest first.
/// Each entry: `(version_stamp, byte_len)`.
pub fn versions_of(harness_dir: &Path, kind: &str, name: &str) -> Vec<(String, u64)> {
    let dir = kind_dir(harness_dir, kind);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !file.starts_with(&prefix) {
            continue;
        }
        // file = "<name>.<stamp>.<ext>"
        let rest = &file[prefix.len()..];
        let Some(dot) = rest.rfind('.') else {
            continue;
        };
        let stamp = &rest[..dot];
        let len = path.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((stamp.to_string(), len));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    out
}

/// Restore an asset to a specific version. `version` is the stamp
/// returned by `versions_of`. Writes the content back over the asset.
/// Returns an error if the version file is missing.
pub fn restore(
    harness_dir: &Path,
    kind: &str,
    name: &str,
    version: &str,
) -> anyhow::Result<String> {
    let ext = match kind {
        "tools" => "lua",
        _ => "md",
    };
    let path = kind_dir(harness_dir, kind).join(format!("{name}.{version}.{ext}"));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("version {version} of {kind}/{name} not found: {e}"))?;

    let target = match kind {
        "prompt" => harness_dir.join("system_prompt.md"),
        "skills" => harness_dir.join("skills").join(format!("{name}.md")),
        "tools" => harness_dir.join("tools").join(format!("{name}.lua")),
        other => anyhow::bail!("unknown asset kind: {other}"),
    };
    std::fs::write(&target, &content)?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sigil-versions-{name}-{}", now_stamp()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        dir
    }

    #[test]
    fn backups_accumulate_and_list_newest_first() {
        let dir = fresh("acc");
        let skill = dir.join("skills").join("math.md");
        std::fs::write(&skill, "v1").unwrap();

        backup(&dir, "skills", "math", "v1");
        std::fs::write(&skill, "v2").unwrap();
        backup(&dir, "skills", "math", "v2");

        let versions = versions_of(&dir, "skills", "math");
        assert_eq!(versions.len(), 2);
        // Newest (v2's snapshot) sorts first.
        assert_eq!(versions[0].1, 2);
        assert_eq!(versions[1].1, 2);

        // Restore works and writes the content back over the live file.
        let v = &versions[0].0;
        let restored = restore(&dir, "skills", "math", v).unwrap();
        assert_eq!(restored, "v2");
        let on_disk = std::fs::read_to_string(&skill).unwrap();
        assert_eq!(on_disk, "v2");
    }

    #[test]
    fn restore_missing_version_errors() {
        let dir = fresh("missing");
        assert!(restore(&dir, "skills", "math", "19700101-000000-000").is_err());
    }

    #[test]
    fn missing_asset_has_no_versions() {
        let dir = fresh("missing-asset");
        // An asset that was never backed up has an empty history.
        assert!(versions_of(&dir, "skills", "brandnew").is_empty());
        // And the kind dir may not even exist yet.
        assert!(versions_of(&dir, "prompt", "system_prompt").is_empty());
    }
}
