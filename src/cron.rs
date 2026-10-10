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
#[cfg(test)]
pub fn save(path: &Path, jobs: &[CronJob]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::harness::lock::with_dir_lock(parent, || save_unlocked(path, jobs))
}

fn save_unlocked(path: &Path, jobs: &[CronJob]) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(jobs)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Mutate the latest persisted schedule under its directory lock.
pub fn update<T>(
    path: &Path,
    f: impl FnOnce(&mut Vec<CronJob>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::harness::lock::with_dir_lock(parent, || {
        let mut jobs = load(path);
        let result = f(&mut jobs)?;
        save_unlocked(path, &jobs)?;
        Ok(result)
    })
}

/// Merge completed execution bookkeeping without resurrecting removed jobs
/// or overwriting scheduling changes made while the model was running.
pub fn record_run(path: &Path, job: &CronJob, now: u64) -> anyhow::Result<()> {
    update(path, |jobs| {
        if let Some(current) = jobs.iter_mut().find(|j| j.id == job.id) {
            let schedule_unchanged = current.status == "active"
                && current.next_run_at == job.next_run_at
                && current.kind == job.kind
                && current.interval_secs == job.interval_secs
                && current.at == job.at;
            if schedule_unchanged {
                *current = after_run(current.clone(), now);
            } else {
                current.last_run_at = Some(now);
                current.run_count += 1;
            }
        }
        Ok(())
    })
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
    fn finishing_run_preserves_schedule_edits() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-cron-merge-{}",
            crate::session::chrono_like_timestamp()
        ));
        let path = dir.join("cron.json");
        let original = job("interval", Some(60), None, "active");
        let mut paused = original.clone();
        paused.status = "paused".into();
        let mut added = original.clone();
        added.id = "new".into();
        save(&path, &[paused, added]).unwrap();
        record_run(&path, &original, 200).unwrap();
        let current = load(&path);
        assert_eq!(current.len(), 2);
        assert_eq!(current[0].status, "paused");
        assert_eq!(current[0].run_count, 1);
        save(&path, &current[1..]).unwrap();
        record_run(&path, &original, 300).unwrap();
        assert_eq!(load(&path).len(), 1, "removed job must not return");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_schedule_edits_are_serialized() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-cron-edits-{}",
            crate::session::chrono_like_timestamp()
        ));
        let path = dir.join("cron.json");
        save(&path, &[]).unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    update(&path, |jobs| {
                        let mut new = job("interval", Some(60), None, "active");
                        new.id = format!("job-{}", jobs.len());
                        jobs.push(new);
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let jobs = load(&path);
        assert_eq!(jobs.len(), 8);
        assert_eq!(
            jobs.iter()
                .map(|j| &j.id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            8
        );
        std::fs::remove_dir_all(dir).unwrap();
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
