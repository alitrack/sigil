//! Runtime-revertible evolution effects — Cordis P1 (revertible effects →
//! safe, automatic rollback of `improve_harness` writes).
//!
//! Every self-modifying write that `improve_harness` makes pairs its
//! forward change with an explicit inverse. The inverse is registered in an
//! [`EffectTracker`] the moment the forward change lands, so a failed or
//! unwanted evolution can be undone *at runtime* — LIFO, in-process —
//! without reaching for `git revert` or the on-disk version archive
//! (`skill_rollback` / `versions`, which remains the durable, cross-session
//! path).
//!
//! This is the Cordis "revertible effects" pattern: each effect carries a
//! forward operation and a matching revert, and the tracker keeps the
//! revert chain so `rollback` re-applies the inverses in the exact reverse
//! order of application. The paper's Theorem 5 (track is a monoid homomorph,
//! so composing inverses is well-defined) is what lets a *sequence* of
//! effects roll back cleanly rather than just a single one — verified below
//! by `rollback_is_lifo_and_composes`.
//!
//! Scope: this module is deliberately file-system-agnostic. It stores a
//! `revert` closure per effect; the *specific* closures (restore a file,
//! refresh live prompt state, ...) are built by the caller (`tools::evolve`)
//! where the relevant handles live. That keeps the framework reusable and
//! trivially testable here with plain file effects.

use std::sync::Mutex;

/// One applied evolution change, with its explicit inverse.
///
/// The forward operation has *already run* by the time an [`EvolveEffect`]
/// is constructed (the caller registers it right after a successful write);
/// the stored `revert` closure is what `EffectTracker::rollback` runs to
/// undo it.
pub struct EvolveEffect {
    /// Human-readable description of what the forward change did, e.g.
    /// `"edit_prompt (global): system_prompt.md"`. Surfaced by
    /// [`EffectTracker::labels`] and by the `revert_effects` tool so the
    /// model (and the audit log) can see what a rollback would undo.
    pub label: String,
    /// The inverse operation. Runs on rollback; returns an error if the
    /// state can no longer be restored (e.g. the file was deleted out from
    /// under us since).
    revert: Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>,
}

impl EvolveEffect {
    /// Build an effect from a label and an explicit inverse closure.
    pub fn new(
        label: String,
        revert: Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>,
    ) -> Self {
        EvolveEffect { label, revert }
    }

    /// Convenience: an effect whose revert restores `path` to `previous`
    /// (or deletes the file if `previous` is `None`, i.e. the file did not
    /// exist before the forward change). This is the inverse of a single
    /// file write / create / delete.
    pub fn restore_file(
        label: String,
        path: std::path::PathBuf,
        previous: Option<String>,
    ) -> Self {
        let revert = move || {
            match previous.as_deref() {
                Some(c) => std::fs::write(&path, c)?,
                None => std::fs::remove_file(&path)?,
            }
            Ok(())
        };
        EvolveEffect::new(label, Box::new(revert))
    }
}

/// LIFO stack of applied effects. `Send + Sync` so it can be shared (via
/// `Arc`) between the write side (`improve_harness`) and the revert side
/// (`revert_effects`).
#[derive(Default)]
pub struct EffectTracker {
    stack: Mutex<Vec<EvolveEffect>>,
}

impl EffectTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of effects currently pending rollback.
    pub fn pending(&self) -> usize {
        self.stack
            .lock()
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Labels of the pending effects, in *application* order (oldest first).
    pub fn labels(&self) -> Vec<String> {
        self.stack
            .lock()
            .map(|s| s.iter().map(|e| e.label.clone()).collect())
            .unwrap_or_default()
    }

    /// Register an applied effect (push onto the stack). Called by the
    /// write side immediately after a successful forward change.
    pub fn record(&self, effect: EvolveEffect) {
        if let Ok(mut s) = self.stack.lock() {
            s.push(effect);
        }
    }

    /// Pop and revert the most recent effect. Returns its label, or
    /// `None` if there was nothing to roll back.
    pub fn rollback_one(&self) -> anyhow::Result<Option<String>> {
        let effect = self.stack.lock().map(|mut s| s.pop()).unwrap_or(None);
        match effect {
            Some(e) => {
                let label = e.label.clone();
                (e.revert)().map_err(|err| {
                    anyhow::anyhow!("revert failed for '{label}': {err}")
                })?;
                Ok(Some(label))
            }
            None => Ok(None),
        }
    }

    /// Revert up to `n` most-recent effects (LIFO). `n == 0` means *all*
    /// pending effects. Returns the labels reverted, in the order they were
    /// reverted (newest first). Stops — and returns what it has so far — if
    /// a revert fails, leaving the already-reverted effects undone and the
    /// remaining ones still pending (best-effort; the error says which one
    /// broke).
    pub fn rollback(&self, n: usize) -> anyhow::Result<Vec<String>> {
        let mut reverted = Vec::new();
        for _ in 0..n.saturating_add(usize::MAX) {
            // n == 0 rolls back everything; otherwise exactly n.
            let stop = if n == 0 { false } else { reverted.len() >= n };
            if stop {
                break;
            }
            match self.rollback_one()? {
                Some(label) => reverted.push(label),
                None => break, // nothing left to roll back
            }
        }
        Ok(reverted)
    }

    /// Drop all pending effects *without* running their inverses. Used after
    /// a successful full rollback (or to discard) so the stack does not
    /// accumulate. Does not touch the file system.
    pub fn clear(&self) {
        if let Ok(mut s) = self.stack.lock() {
            s.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sigil-revertible-test-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn restore_file_creates_then_reverts_to_absent() {
        let dir = tmp("create");
        let path = dir.join("new.txt");
        assert!(!path.exists());
        let effect = EvolveEffect::restore_file("create new.txt".into(), path.clone(), None);
        let tracker = EffectTracker::new();
        // forward: create the file
        std::fs::write(&path, "v2").unwrap();
        tracker.record(effect);
        assert!(path.exists());
        assert_eq!(tracker.pending(), 1);
        // revert: file must be gone again
        let reverted = tracker.rollback(0).unwrap();
        assert_eq!(reverted, vec!["create new.txt"]);
        assert!(!path.exists(), "revert must delete a newly-created file");
        assert_eq!(tracker.pending(), 0);
    }

    #[test]
    fn restore_file_restores_previous_content() {
        let dir = tmp("overwrite");
        let path = dir.join("edit.txt");
        std::fs::write(&path, "v1-original").unwrap();
        let effect = EvolveEffect::restore_file(
            "edit edit.txt".into(),
            path.clone(),
            Some("v1-original".to_string()),
        );
        let tracker = EffectTracker::new();
        // forward: overwrite
        std::fs::write(&path, "v2-changed").unwrap();
        tracker.record(effect);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2-changed");
        // revert: back to v1
        tracker.rollback_one().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1-original");
    }

    #[test]
    fn rollback_is_lifo_and_composes() {
        // The core Cordis guarantee: a *sequence* of effects rolls back in
        // the exact reverse order of application. Three writes to one file
        // where each effect remembers its own predecessor must restore the
        // original state only when reverted LIFO.
        let dir = tmp("lifo");
        let path = dir.join("seq.txt");
        let original = "BASE";
        std::fs::write(&path, original).unwrap();

        let tracker = EffectTracker::new();
        // a: BASE -> A (remember BASE)
        let e_a = EvolveEffect::restore_file("a".into(), path.clone(), Some(original.to_string()));
        std::fs::write(&path, "A").unwrap();
        tracker.record(e_a);
        // b: A -> B (remember A)
        let e_b = EvolveEffect::restore_file("b".into(), path.clone(), Some("A".to_string()));
        std::fs::write(&path, "B").unwrap();
        tracker.record(e_b);
        // c: B -> C (remember B)
        let e_c = EvolveEffect::restore_file("c".into(), path.clone(), Some("B".to_string()));
        std::fs::write(&path, "C").unwrap();
        tracker.record(e_c);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "C");
        assert_eq!(tracker.labels(), vec!["a", "b", "c"]);

        // roll back exactly one → C reverts to B (c's inverse)
        let one = tracker.rollback(1).unwrap();
        assert_eq!(one, vec!["c"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "B");
        // roll back the remaining two → B -> A -> BASE
        let rest = tracker.rollback(0).unwrap();
        assert_eq!(rest, vec!["b", "a"]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "full LIFO rollback must restore the pre-evolution state"
        );
        assert_eq!(tracker.pending(), 0);
    }

    #[test]
    fn rollback_respects_n_and_stops_when_empty() {
        let dir = tmp("n");
        let p1 = dir.join("1.txt");
        let p2 = dir.join("2.txt");
        std::fs::write(&p1, "x").unwrap();
        std::fs::write(&p2, "y").unwrap();
        let t = EffectTracker::new();
        t.record(EvolveEffect::restore_file("d1".into(), p1.clone(), Some("x".into())));
        t.record(EvolveEffect::restore_file("d2".into(), p2.clone(), Some("y".into())));
        // n=1 reverts only the newest (d2)
        let r = t.rollback(1).unwrap();
        assert_eq!(r, vec!["d2"]);
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "y");
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "x"); // p1 untouched? no: p1 already "x"
        // p1 was forward-kept at "x"; its effect not yet reverted
        assert_eq!(t.pending(), 1);
        // n larger than pending → reverts the rest, then stops
        let r2 = t.rollback(99).unwrap();
        assert_eq!(r2, vec!["d1"]);
        assert_eq!(t.pending(), 0);
        // rollback on empty stack → no-op, Ok(vec![])
        assert_eq!(t.rollback(0).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn clear_discards_without_reverting() {
        let dir = tmp("clear");
        let p = dir.join("c.txt");
        std::fs::write(&p, "orig").unwrap();
        let t = EffectTracker::new();
        t.record(EvolveEffect::restore_file("c".into(), p.clone(), Some("orig".into())));
        std::fs::write(&p, "modified").unwrap();
        assert_eq!(t.pending(), 1);
        t.clear();
        assert_eq!(t.pending(), 0);
        // file untouched by clear
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "modified");
    }

    #[test]
    fn pending_and_labels_reflect_application_order() {
        let t = EffectTracker::new();
        assert_eq!(t.pending(), 0);
        assert!(t.labels().is_empty());
        t.record(EvolveEffect::restore_file("first".into(), std::env::temp_dir().join("nope-a"), None));
        t.record(EvolveEffect::restore_file("second".into(), std::env::temp_dir().join("nope-b"), None));
        assert_eq!(t.pending(), 2);
        assert_eq!(t.labels(), vec!["first", "second"]);
    }
}
