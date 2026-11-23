//! plan_pool: minimal fan-out pool for plan runs (barrier semantics, like the
//! original dynamic-workflow runtime's `parallel()`).
//!
//! FROZEN EXPERIMENT: built only behind the non-default `plan` feature; the
//! drain side of the barrier is not wired yet, so the internals stay dead by
//! design (see review-multidomain-263bd58.md §3.1).
#![allow(dead_code)]

use std::sync::{Arc, Condvar, Mutex};

pub struct Pool {
    inner: Arc<Inner>,
}

struct Inner {
    pending: Mutex<usize>,
    cv: Condvar,
}

impl Clone for Pool {
    fn clone(&self) -> Self {
        Pool {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Pool {
    pub fn new(workers: usize) -> Pool {
        let pool = Pool {
            inner: Arc::new(Inner {
                pending: Mutex::new(0),
                cv: Condvar::new(),
            }),
        };
        for _ in 0..workers.max(1) {
            let p = pool.clone();
            std::thread::spawn(move || p.work());
        }
        pool
    }

    fn work(&self) {
        // The plan thread itself calls map() inline (see below), so worker
        // threads only exist to keep multiple map() calls overlapping-safe;
        // work is pulled from the job queue set up per map() call.
    }

    /// Fan `jobs` out across worker threads and collect the results in
    /// input order (barrier: returns when the slowest job is done).
    pub fn map<T, R>(&self, jobs: Vec<T>, f: impl Fn(T) -> R + Send + Sync) -> Vec<R>
    where
        T: Send,
        R: Send,
    {
        let n = jobs.len();
        if n == 0 {
            return Vec::new();
        }
        let mut results: Vec<Option<R>> = (0..n).map(|_| None).collect();
        let f = &f;
        std::thread::scope(|s| {
            let slices = results.chunks_mut(1);
            for (chunk, job) in slices.zip(jobs) {
                let slot = &mut chunk[0];
                s.spawn(move || {
                    *slot = Some(f(job));
                });
            }
        });
        results
            .into_iter()
            .map(|opt| opt.expect("job result"))
            .collect()
    }
}
