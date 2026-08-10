//! Persistent goals: the model declares what it is working toward and
//! sigil keeps active goals visible in every turn's system context.
//!
//! Mirrors prime-agent's `GoalState` (objective + status + budget)
//! minus the per-turn token accounting: sigil's goals are durable
//! across sessions (`harness_dir/goals.jsonl`) and surfaced to the
//! model every turn until completed.

use serde::{Deserialize, Serialize};

/// Goal lifecycle: active → paused ⇄ active → complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "complete")]
    Complete,
}

/// One persistent goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub created_ts: String,
    pub updated_ts: String,
}

impl GoalState {
    pub fn new(id: String, objective: String) -> Self {
        let ts = crate::session::chrono_like_timestamp();
        Self {
            id,
            objective,
            status: GoalStatus::Active,
            created_ts: ts.clone(),
            updated_ts: ts,
        }
    }
}

/// Load goals from `dir/goals.jsonl` (one JSON per line, newest last).
/// Corrupt lines are skipped — the file degrades gracefully.
pub fn load_goals(dir: &std::path::Path) -> Vec<GoalState> {
    let path = dir.join("goals.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<GoalState>(l.trim()).ok())
        .collect()
}

/// Persist goals atomically (tmp + rename) under the directory lock.
pub fn save_goals(dir: &std::path::Path, goals: &[GoalState]) -> anyhow::Result<()> {
    let path = dir.join("goals.jsonl");
    let tmp = dir.join("goals.jsonl.tmp");
    let mut out = String::new();
    for g in goals {
        if let Ok(line) = serde_json::to_string(g) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    crate::harness::lock::with_dir_lock(dir, || {
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })
}

/// Format active goals as a system-context block, or `None` when there
/// are no active goals (nothing to inject).
pub fn active_goals_block(goals: &[GoalState]) -> Option<String> {
    let active: Vec<&GoalState> = goals
        .iter()
        .filter(|g| g.status == GoalStatus::Active)
        .collect();
    if active.is_empty() {
        return None;
    }
    let mut out = String::from("## Active goals\n");
    for g in &active {
        out.push_str(&format!("- `{}`: {}\n", g.id, g.objective));
    }
    out.push_str(
        "Keep working toward these. When one is done, call manage_goals with action=complete on its id.",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_goals_block_lists_only_active() {
        let mut g1 = GoalState::new("g1".into(), "finish the report".into());
        let g2 = GoalState::new("g2".into(), "ship the release".into());
        g1.status = GoalStatus::Complete;
        let g3 = GoalState::new("g3".into(), "paused one".into());
        let g3 = {
            let mut x = g3;
            x.status = GoalStatus::Paused;
            x
        };
        let block = active_goals_block(&[g1, g2, g3]).unwrap();
        assert!(block.contains("ship the release"));
        assert!(!block.contains("finish the report"));
        assert!(!block.contains("paused one"));
        assert!(block.contains("manage_goals"));
    }

    #[test]
    fn active_goals_block_none_when_nothing_active() {
        let mut g = GoalState::new("g1".into(), "done".into());
        g.status = GoalStatus::Complete;
        assert!(active_goals_block(&[g]).is_none());
        assert!(active_goals_block(&[]).is_none());
    }

    #[test]
    fn goals_roundtrip_via_jsonl() {
        let dir = std::env::temp_dir().join(format!("sigil-goals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let g1 = GoalState::new("g1".into(), "first".into());
        let mut g2 = GoalState::new("g2".into(), "second".into());
        g2.status = GoalStatus::Complete;
        save_goals(&dir, &[g1.clone(), g2.clone()]).unwrap();

        let loaded = load_goals(&dir);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "g1");
        assert_eq!(loaded[0].objective, "first");
        assert_eq!(loaded[0].status, GoalStatus::Active);
        assert_eq!(loaded[1].status, GoalStatus::Complete);

        // Corrupt line degrades gracefully.
        let path = dir.join("goals.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(load_goals(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
