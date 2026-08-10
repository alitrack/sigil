//! `manage_goals` — the model declares persistent goals and marks them
//! done. Active goals are injected into the system context every turn
//! (see `agent::run_messages`), so long-running work stays on the
//! rails across sessions and even across sigil instances (the goals
//! file lives in the harness dir).

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};

use crate::goals::{GoalState, GoalStatus, save_goals};
use crate::tools::{Tool, ToolOutput};

/// Tool that manages persistent goals: add, complete, pause, resume,
/// list, clear.
pub struct ManageGoalsTool {
    /// Where `goals.jsonl` lives (the harness dir).
    pub goals_dir: std::path::PathBuf,
    /// In-memory mirror shared with the agent loop (injected into the
    /// system context each turn).
    pub goals: Arc<RwLock<Vec<GoalState>>>,
}

impl ManageGoalsTool {
    fn with_goals<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Vec<GoalState>) -> anyhow::Result<T>,
    {
        let mut goals = self
            .goals
            .write()
            .map_err(|e| anyhow::anyhow!("goals lock poisoned: {e}"))?;
        let result = f(&mut goals)?;
        save_goals(&self.goals_dir, &goals)?;
        Ok(result)
    }
}

#[async_trait]
impl Tool for ManageGoalsTool {
    fn name(&self) -> &str {
        "manage_goals"
    }

    fn description(&self) -> &str {
        "Manage persistent goals — the long-running objectives you are \
         working toward. Actions: 'add' (objective: one clear sentence), \
         'complete' (id: mark a goal done), 'pause' (id), 'resume' (id), \
         'list' (show all with status), 'clear' (drop every goal). \
         Active goals are injected into your system context every turn \
         until completed, so declaring a goal keeps long tasks on the \
         rails across sessions. Complete goals when their objective is \
         genuinely met."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "complete", "pause", "resume", "list", "clear"],
                    "description": "What to do"
                },
                "objective": {
                    "type": "string",
                    "description": "For add: one clear sentence describing the goal."
                },
                "turn_budget": {
                    "type": "integer",
                    "description": "For add (optional): auto-pause the goal after this many agent turns (one LLM call = one turn)."
                },
                "id": {
                    "type": "string",
                    "description": "Goal id (shown by 'list') — required for complete/pause/resume."
                }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match action {
            "add" => {
                let objective = args
                    .get("objective")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("add requires a non-empty 'objective'"))?;
                if objective.chars().count() < 8 {
                    anyhow::bail!(
                        "objective too short ({} chars, min 8) — describe the goal in one clear sentence",
                        objective.chars().count()
                    );
                }
                let id = format!("g{}", crate::session::chrono_like_timestamp());
                // Optional turn budget: auto-pause after N agent turns.
                let budget = args
                    .get("turn_budget")
                    .and_then(Value::as_u64)
                    .filter(|b| *b > 0);
                self.with_goals(|goals| {
                    let goal = match budget {
                        Some(b) => {
                            GoalState::with_turn_budget(id.clone(), objective.to_string(), b)
                        }
                        None => GoalState::new(id.clone(), objective.to_string()),
                    };
                    goals.push(goal);
                    Ok(())
                })?;
                Ok(ToolOutput {
                    text: format!(
                        "goal '{id}' added and now injected into your context every turn: {objective}"
                    ),
                })
            }
            "complete" | "pause" | "resume" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("{action} requires 'id'"))?;
                let status = match action {
                    "complete" => GoalStatus::Complete,
                    "pause" => GoalStatus::Paused,
                    _ => GoalStatus::Active,
                };
                let changed = self.with_goals(|goals| {
                    let Some(g) = goals.iter_mut().find(|g| g.id == id) else {
                        anyhow::bail!("no goal with id '{id}' (see 'list')");
                    };
                    let was = g.status;
                    g.status = status;
                    g.updated_ts = crate::session::chrono_like_timestamp();
                    Ok(was != status)
                })?;
                if changed {
                    Ok(ToolOutput {
                        text: format!("goal '{id}' marked {status:?}"),
                    })
                } else {
                    Ok(ToolOutput {
                        text: format!("goal '{id}' was already {status:?}"),
                    })
                }
            }
            "list" => {
                let goals = self
                    .goals
                    .read()
                    .map_err(|e| anyhow::anyhow!("goals lock poisoned: {e}"))?;
                if goals.is_empty() {
                    return Ok(ToolOutput {
                        text: "no goals yet — use action=add to declare one".to_string(),
                    });
                }
                let mut text = String::from("goals:");
                for g in goals.iter() {
                    text.push_str(&format!(
                        "\n- {} [{}] — {}",
                        g.id,
                        match g.status {
                            GoalStatus::Active => "active",
                            GoalStatus::Paused => "paused",
                            GoalStatus::Complete => "complete",
                        },
                        g.objective
                    ));
                    if let Some(b) = g.turn_budget {
                        text.push_str(&format!(" (turns {}/{})", g.turns_used, b));
                    }
                }
                Ok(ToolOutput { text })
            }
            "clear" => {
                self.with_goals(|goals| {
                    goals.clear();
                    Ok(())
                })?;
                Ok(ToolOutput {
                    text: "all goals cleared".to_string(),
                })
            }
            _ => anyhow::bail!("unknown action '{action}' (add|complete|pause|resume|list|clear)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::load_goals;
    use crate::tools::Tool;

    fn fresh_dir(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("sigil-goal-tool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir)
    }

    #[tokio::test]
    async fn add_list_complete_flow() {
        let (dir, _) = fresh_dir("flow");
        let goals: Arc<RwLock<Vec<GoalState>>> = Arc::new(RwLock::new(Vec::new()));
        let tool = ManageGoalsTool {
            goals_dir: dir.clone(),
            goals: goals.clone(),
        };

        let out = tool
            .run(&json!({
                "action": "add",
                "objective": "ship the v2 release before Friday"
            }))
            .await
            .unwrap();
        assert!(out.text.contains("injected"), "{}", out.text);

        let listed = tool.run(&json!({ "action": "list" })).await.unwrap();
        assert!(
            listed.text.contains("ship the v2 release"),
            "{}",
            listed.text
        );

        // Persisted to disk.
        let on_disk = load_goals(&dir);
        assert_eq!(on_disk.len(), 1);
        let id = on_disk[0].id.clone();

        // Complete it → no longer active.
        tool.run(&json!({ "action": "complete", "id": id }))
            .await
            .unwrap();
        let on_disk = load_goals(&dir);
        assert_eq!(on_disk[0].status, GoalStatus::Complete);

        // Pause/resume on a fresh goal.
        let out = tool
            .run(&json!({ "action": "add", "objective": "second objective here" }))
            .await
            .unwrap();
        let id2 = out.text.split('\'').nth(1).unwrap().to_string();
        tool.run(&json!({ "action": "pause", "id": id2 }))
            .await
            .unwrap();
        assert_eq!(load_goals(&dir)[1].status, GoalStatus::Paused);
        tool.run(&json!({ "action": "resume", "id": id2 }))
            .await
            .unwrap();
        assert_eq!(load_goals(&dir)[1].status, GoalStatus::Active);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn add_with_turn_budget_parses_and_lists() {
        let (dir, _) = fresh_dir("budget");
        let goals: Arc<RwLock<Vec<GoalState>>> = Arc::new(RwLock::new(Vec::new()));
        let tool = ManageGoalsTool {
            goals_dir: dir.clone(),
            goals: goals.clone(),
        };

        let out = tool
            .run(&json!({
                "action": "add",
                "objective": "refactor the core module",
                "turn_budget": 3,
            }))
            .await
            .unwrap();
        let _id = out.text.split('\'').nth(1).unwrap().to_string();
        let on_disk = load_goals(&dir);
        assert_eq!(on_disk[0].turn_budget, Some(3));
        assert_eq!(on_disk[0].turns_used, 0);

        // list shows the budget progress.
        let listed = tool.run(&json!({ "action": "list" })).await.unwrap();
        assert!(listed.text.contains("(turns 0/3)"), "{}", listed.text);

        // Zero/negative budgets are treated as "no budget".
        let out = tool
            .run(&json!({
                "action": "add",
                "objective": "another goal without a budget",
                "turn_budget": 0,
            }))
            .await
            .unwrap();
        let id2 = out.text.split('\'').nth(1).unwrap().to_string();
        let on_disk = load_goals(&dir);
        let g2 = on_disk.iter().find(|g| g.id == id2).unwrap();
        assert_eq!(g2.turn_budget, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_noise_and_unknown_ids() {
        let (dir, _) = fresh_dir("noise");
        let goals: Arc<RwLock<Vec<GoalState>>> = Arc::new(RwLock::new(Vec::new()));
        let tool = ManageGoalsTool {
            goals_dir: dir.clone(),
            goals: goals.clone(),
        };

        // Too-short objective rejected.
        let err = tool
            .run(&json!({ "action": "add", "objective": "hi" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too short"), "{err}");

        // Unknown id rejected, file untouched.
        let err = tool
            .run(&json!({ "action": "complete", "id": "nope" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no goal"), "{err}");

        // Unknown action rejected.
        let err = tool.run(&json!({ "action": "explode" })).await.unwrap_err();
        assert!(err.to_string().contains("unknown action"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
