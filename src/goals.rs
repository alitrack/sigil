//! Persistent goals: the model declares what it is working toward and
//! sigil keeps active goals visible in every turn's system context.
//!
//! Mirrors prime-agent's `GoalState` (objective + status + budget).
//! The provider reports no token usage, so the budget is counted in
//! **agent turns** (one LLM call = one turn): an optional `turn_budget`
//! auto-pauses the goal once its turns are used up.

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
    /// Optional turn budget: the goal auto-pauses once this many agent
    /// turns have been spent on it (`None` = unlimited).
    #[serde(default)]
    pub turn_budget: Option<u64>,
    /// Agent turns spent on this goal while it was active.
    #[serde(default)]
    pub turns_used: u64,
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
            turn_budget: None,
            turns_used: 0,
        }
    }

    pub fn with_turn_budget(id: String, objective: String, budget: u64) -> Self {
        let mut g = Self::new(id, objective);
        g.turn_budget = Some(budget);
        g
    }
}

/// Spend one turn on every active goal. Returns the number of goals
/// whose budget became exhausted as a result.
pub fn bump_turn_usage(goals: &mut [GoalState]) -> usize {
    let mut exhausted = 0;
    for g in goals.iter_mut() {
        if g.status == GoalStatus::Active {
            g.turns_used += 1;
            if g.turn_budget.is_some_and(|b| g.turns_used >= b) {
                exhausted += 1;
            }
        }
    }
    exhausted
}

/// Auto-pause active goals whose turn budget is exhausted (so the
/// model stops being nudged toward a goal that ran out of runway).
/// Returns how many goals were paused.
pub fn pause_exhausted(goals: &mut [GoalState]) -> usize {
    let mut paused = 0;
    for g in goals.iter_mut() {
        if g.status == GoalStatus::Active && g.turn_budget.is_some_and(|b| g.turns_used >= b) {
            g.status = GoalStatus::Paused;
            g.updated_ts = crate::session::chrono_like_timestamp();
            paused += 1;
        }
    }
    paused
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

/// Marker prefix of the injected goals block. `agent::run_messages`
/// uses it to *replace* any previously injected block instead of
/// appending a duplicate each loop iteration.
pub const GOALS_MARKER: &str = "## Active goals";

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
    let mut out = String::from(GOALS_MARKER);
    out.push('\n');
    for g in &active {
        out.push_str(&format!("- `{}`: {}", g.id, g.objective));
        if let Some(b) = g.turn_budget {
            out.push_str(&format!(" (turns {}/{})", g.turns_used, b));
        }
        out.push('\n');
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
    fn turn_budget_bumps_and_auto_pauses() {
        let g1 = GoalState::with_turn_budget("g1".into(), "ship v2".into(), 2);
        let g2 = GoalState::new("g2".into(), "unlimited".into());
        let mut g3 = GoalState::with_turn_budget("g3".into(), "paused early".into(), 5);
        g3.status = GoalStatus::Paused;

        // Turn 1: both actives bump; budget 2 not yet exhausted.
        assert_eq!(
            bump_turn_usage(&mut [g1.clone(), g2.clone(), g3.clone()]),
            0
        );
        let mut all = vec![g1.clone(), g2.clone(), g3.clone()];
        bump_turn_usage(&mut all);
        assert_eq!(all[0].turns_used, 1);
        assert_eq!(all[1].turns_used, 1);
        assert_eq!(all[2].turns_used, 0, "paused goals don't spend turns");
        assert_eq!(pause_exhausted(&mut all), 0);

        // While active, a budgeted goal's progress shows in the block.
        let block = active_goals_block(&all).unwrap();
        assert!(block.contains("(turns 1/2)"), "{block}");
        assert!(block.contains("unlimited"));

        // Turn 2: g1 exhausts (2/2) → auto-paused; g2 keeps counting.
        bump_turn_usage(&mut all);
        assert_eq!(pause_exhausted(&mut all), 1);
        assert_eq!(all[0].status, GoalStatus::Paused);
        assert_eq!(all[1].status, GoalStatus::Active);
        assert_eq!(all[1].turns_used, 2);

        // Exhausted goals no longer appear in the injected block.
        let block = active_goals_block(&all).unwrap();
        assert!(!block.contains("ship v2"));
        assert!(block.contains("unlimited"));
    }

    #[test]
    fn old_goal_lines_without_budget_fields_load() {
        // Pre-budget goals.jsonl lines must still parse (serde default).
        let dir = std::env::temp_dir().join(format!("sigil-goals-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("goals.jsonl"),
            "{\"id\":\"g1\",\"objective\":\"legacy goal\",\"status\":\"active\",\"created_ts\":\"t\",\"updated_ts\":\"t\"}\n",
        )
        .unwrap();
        let loaded = load_goals(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].turn_budget, None);
        assert_eq!(loaded[0].turns_used, 0);
        let _ = std::fs::remove_dir_all(&dir);
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
