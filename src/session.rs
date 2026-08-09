//! Session persistence: REPL turns are appended to a JSONL file
//! (`~/.sigil/sessions/<timestamp>.jsonl`); `--resume <id>` loads a
//! session's history so a conversation continues across invocations.
//!
//! Format: one JSON object per line — `{"role": "user"|"assistant",
//! "text": "..."}`. Sessions are plain text history (the harness and
//! memory tools provide the durable knowledge store; this is the
//! conversational transcript).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Default sessions directory.
pub fn default_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sigil")
        .join("sessions")
}

/// A single recorded turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    pub role: String, // "user" | "assistant"
    pub text: String,
}

/// Start a new session file, returning its id (file stem).
pub fn start(sessions_dir: &Path) -> std::io::Result<String> {
    fs::create_dir_all(sessions_dir)?;
    let id = chrono_like_timestamp();
    let path = sessions_dir.join(format!("{id}.jsonl"));
    // Touch the file so the session exists immediately.
    OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(id)
}

/// Append one turn to a session file.
pub fn append(sessions_dir: &Path, id: &str, turn: &Turn) -> std::io::Result<()> {
    let path = sessions_dir.join(format!("{id}.jsonl"));
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(turn).map_err(std::io::Error::other)?;
    writeln!(f, "{line}")
}

/// Load a session's turns (empty if the session doesn't exist).
pub fn load(sessions_dir: &Path, id: &str) -> std::io::Result<Vec<Turn>> {
    let path = sessions_dir.join(format!("{id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(path)?;
    let mut turns = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(t) = serde_json::from_str::<Turn>(&line) {
            turns.push(t);
        }
    }
    Ok(turns)
}

/// List session ids (newest first) for `--resume` discovery.
pub fn list(sessions_dir: &Path) -> std::io::Result<Vec<String>> {
    let mut ids = Vec::new();
    if !sessions_dir.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl") {
            ids.push(name.trim_end_matches(".jsonl").to_string());
        }
    }
    ids.sort();
    ids.reverse();
    Ok(ids)
}

/// Compact timestamp: YYYYMMDD-HHMMSS-nnnnnnnnn (nanosecond precision
/// so sessions started in the same second never collide).
fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Days since 1970-01-01 → approximate date (ignores leap years' drift
    // beyond 2026; good enough for file naming).
    let y = 1970 + days / 365;
    format!("{y}-{days:05}-{h:02}{m:02}{s:02}-{:09}", now.subsec_nanos())
}

/// Portable session archive: metadata + the full transcript. Written by
/// `export_session`, read by `import_session`. The turns array is what
/// `--resume` replays.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionArchive {
    /// Sigil version that produced the export.
    pub version: String,
    /// Export timestamp.
    pub exported_at: String,
    /// Original session id (informational).
    pub source_id: String,
    /// Transcript.
    pub turns: Vec<Turn>,
}

/// Export a session to a portable JSON archive file.
pub fn export_session(
    sessions_dir: &Path,
    id: &str,
    out_path: &Path,
) -> std::io::Result<SessionArchive> {
    let turns = load(sessions_dir, id)?;
    if turns.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {id} not found or empty"),
        ));
    }
    let archive = SessionArchive {
        version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono_like_timestamp(),
        source_id: id.to_string(),
        turns,
    };
    let json = serde_json::to_string_pretty(&archive)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, json)?;
    Ok(archive)
}

/// Import a session archive into the sessions directory as a **new**
/// session (so the original is never overwritten). Returns the new id.
pub fn import_session(sessions_dir: &Path, in_path: &Path) -> std::io::Result<String> {
    let json = fs::read_to_string(in_path)?;
    let archive: SessionArchive = serde_json::from_str(&json).map_err(std::io::Error::other)?;
    if archive.turns.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "archive has no turns",
        ));
    }
    let id = start(sessions_dir)?;
    for turn in &archive.turns {
        append(sessions_dir, &id, turn)?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_append_and_load() {
        let dir = std::env::temp_dir().join(format!("sigil-session-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "hello".into(),
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "hi there".into(),
            },
        )
        .unwrap();

        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].text, "hi there");

        // Missing session → empty.
        assert!(load(&dir, "nope").unwrap().is_empty());

        // List includes it.
        assert!(list(&dir).unwrap().contains(&id));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_import_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-session-xport-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "hello".into(),
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "hi".into(),
            },
        )
        .unwrap();

        // Export to a portable JSON file.
        let out = dir.join("archive.json");
        let archive = export_session(&dir, &id, &out).unwrap();
        assert_eq!(archive.turns.len(), 2);
        assert_eq!(archive.source_id, id);
        assert!(out.exists());

        // Import into the same dir → NEW session id, same turns.
        let new_id = import_session(&dir, &out).unwrap();
        assert_ne!(new_id, id);
        let turns = load(&dir, &new_id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].text, "hi");

        // Exporting a missing session fails cleanly.
        assert!(export_session(&dir, "nope", &dir.join("x.json")).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
