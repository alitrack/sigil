//! Scheduled agent turns — the sigil analogue of prime-agent's
//! cron-jobs.ts (minimal: interval and one-shot schedules, JSON
//! persistence, a daemon tick loop that injects a prompt into a
//! session like a user turn).
//!
//! Time is unix seconds (no chrono dependency); a `once` job fires at
//! its `at` timestamp and then becomes `completed`, an `interval` job
//! refires every `interval_secs` after its previous run.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One scheduled job. Status: active | paused | completed | cancelled.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub status: String,
    /// "interval" (refires every `interval_secs`) or "once" (fires at `at`).
    pub kind: String,
    pub interval_secs: Option<u64>,
    /// Unix seconds for `once` jobs.
    pub at: Option<u64>,
    pub prompt: String,
    /// Target session id; None = the daemon's default session.
    pub session: Option<String>,
    pub created_at: u64,
    pub next_run_at: u64,
    pub last_run_at: Option<u64>,
    pub run_count: u64,
}

impl CronJob {
    /// Unix seconds of the job's next run (a completed/cancelled job
    /// has none — it never fires again). Tests-only convenience:
    /// production scheduling filters on the `next_run_at` field via
    /// `due()`.
    #[allow(dead_code)]
    pub fn next_run_after(&self, now: u64) -> Option<u64> {
        if self.status == "completed" || self.status == "cancelled" {
            return None;
        }
        Some(match self.kind.as_str() {
            "once" => self.at.unwrap_or(now),
            _ => self
                .last_run_at
                .map(|t| t + self.interval_secs.unwrap_or(0))
                .unwrap_or(now + self.interval_secs.unwrap_or(0)),
        })
    }
}

/// Load the job list from `path` (missing file = empty list).
pub fn load(path: &Path) -> Vec<CronJob> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Atomically persist the job list (write temp + rename, like
/// prime-agent's fsync-then-rename).
pub fn save(path: &Path, jobs: &[CronJob]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(jobs)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Current unix seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Jobs that are due at `now` (active and next_run_at <= now).
pub fn due(jobs: &[CronJob], now: u64) -> Vec<CronJob> {
    jobs.iter()
        .filter(|j| j.status == "active" && j.next_run_at <= now)
        .cloned()
        .collect()
}

/// Advance a job after a run: record last_run_at, bump run_count and
/// schedule the next run (interval) or complete it (once).
pub fn after_run(mut job: CronJob, now: u64) -> CronJob {
    job.last_run_at = Some(now);
    job.run_count += 1;
    match job.kind.as_str() {
        "once" => {
            job.status = "completed".to_string();
            job.next_run_at = 0;
        }
        _ => {
            job.next_run_at = now + job.interval_secs.unwrap_or(0);
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: &str, interval: Option<u64>, at: Option<u64>, status: &str) -> CronJob {
        CronJob {
            id: "j1".into(),
            status: status.into(),
            kind: kind.into(),
            interval_secs: interval,
            at,
            prompt: "p".into(),
            session: None,
            created_at: 0,
            next_run_at: 100,
            last_run_at: None,
            run_count: 0,
        }
    }

    #[test]
    fn interval_refires_after_each_run() {
        let j = job("interval", Some(60), None, "active");
        assert_eq!(j.next_run_after(0), Some(60));
        let ran = after_run(j.clone(), 100);
        assert_eq!(ran.run_count, 1);
        assert_eq!(ran.next_run_at, 160);
        assert_eq!(ran.status, "active");
    }

    #[test]
    fn once_completes_after_run() {
        let j = job("once", None, Some(50), "active");
        let ran = after_run(j.clone(), 50);
        assert_eq!(ran.run_count, 1);
        assert_eq!(ran.status, "completed");
        assert_eq!(ran.next_run_after(51), None);
    }

    #[test]
    fn paused_and_completed_never_fire() {
        assert_eq!(
            job("interval", Some(10), None, "paused").next_run_after(0),
            Some(10)
        );
        let j = job("interval", Some(10), None, "paused");
        assert!(
            due(std::slice::from_ref(&j), 1000).is_empty(),
            "paused not due"
        );
        assert!(due(&[job("interval", Some(10), None, "completed")], 1000).is_empty());
    }

    #[test]
    fn due_picks_active_jobs_at_or_before_now() {
        let mut j = job("interval", Some(10), None, "active");
        j.next_run_at = 100;
        assert!(due(&[j.clone()], 99).is_empty());
        assert_eq!(due(&[j.clone()], 100).len(), 1);
        assert_eq!(due(&[j.clone()], 101).len(), 1);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-cron-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cron.json");
        let jobs = vec![job("interval", Some(30), None, "active")];
        save(&path, &jobs).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].kind, "interval");
        assert_eq!(loaded[0].interval_secs, Some(30));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
