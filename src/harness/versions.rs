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
        // For tools, the base dir passed in IS the tools dir (the
        // evolution tools call backup/restore with tools_dir as root),
        // so the live tool lives directly under it.
        "tools" => harness_dir.join(format!("{name}.lua")),
        other => anyhow::bail!("unknown asset kind: {other}"),
    };
    std::fs::write(&target, &content)?;
    Ok(content)
}

/// Prune archived versions of an asset, keeping only the newest
/// `keep` entries. Returns the number of files removed. Ordering is by
/// stamp (newest first), so the oldest versions are dropped first.
pub fn gc(harness_dir: &Path, kind: &str, name: &str, keep: usize) -> usize {
    let dir = kind_dir(harness_dir, kind);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let prefix = format!("{name}.");
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !file.starts_with(&prefix) {
            continue;
        }
        let rest = &file[prefix.len()..];
        let Some(dot) = rest.rfind('.') else {
            continue;
        };
        paths.push((rest[..dot].to_string(), path));
    }
    paths.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let mut removed = 0;
    for (_, path) in paths.iter().skip(keep) {
        if std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Maximum archived versions kept automatically per asset. When a
/// backup pushes an asset past this, the oldest versions are pruned.
pub const AUTO_KEEP: usize = 50;

/// `backup` + automatic pruning: after snapshotting, keep only the
/// newest [`AUTO_KEEP`] versions of this asset.
pub fn backup_and_prune(harness_dir: &Path, kind: &str, name: &str, content: &str) {
    backup(harness_dir, kind, name, content);
    gc(harness_dir, kind, name, AUTO_KEEP);
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

    #[test]
    fn gc_keeps_only_newest_n() {
        let dir = fresh("gc");
        let skill = dir.join("skills").join("math.md");
        for i in 0..5 {
            std::fs::write(&skill, format!("v{i}")).unwrap();
            backup(&dir, "skills", "math", &format!("v{i}"));
        }
        assert_eq!(versions_of(&dir, "skills", "math").len(), 5);

        // Keep the newest 2.
        let removed = gc(&dir, "skills", "math", 2);
        assert_eq!(removed, 3);
        let left = versions_of(&dir, "skills", "math");
        assert_eq!(left.len(), 2);
        // The newest snapshot ("v4") survives.
        let newest = restore(&dir, "skills", "math", &left[0].0).unwrap();
        assert_eq!(newest, "v4");
    }

    #[test]
    fn backup_and_prune_enforces_auto_keep() {
        let dir = fresh("auto");
        let skill = dir.join("skills").join("math.md");
        for i in 0..(AUTO_KEEP + 3) {
            std::fs::write(&skill, format!("v{i}")).unwrap();
            backup_and_prune(&dir, "skills", "math", &format!("v{i}"));
        }
        assert_eq!(versions_of(&dir, "skills", "math").len(), AUTO_KEEP);
    }

    #[test]
    fn restore_tools_kind_writes_live_tool_directly_under_tools_dir() {
        let dir = fresh("tools-kind");
        let tools = dir.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let tool = tools.join("which.lua");
        std::fs::write(&tool, "v1").unwrap();
        backup(&tools, "tools", "which", "v1");
        std::fs::write(&tool, "v2").unwrap();
        backup(&tools, "tools", "which", "v2");

        let versions = versions_of(&tools, "tools", "which");
        assert_eq!(versions.len(), 2);
        // Restore the older version.
        let old_stamp = versions.last().unwrap().0.clone();
        let restored = restore(&tools, "tools", "which", &old_stamp).unwrap();
        assert_eq!(restored, "v1");
        // The live tool file was overwritten, directly under tools dir.
        let on_disk = std::fs::read_to_string(&tool).unwrap();
        assert_eq!(on_disk, "v1");
    }
}
