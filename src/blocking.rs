//! Bounded isolation for filesystem reads and extraction.
//!
//! A timed-out syscall cannot be cancelled. Its slot stays occupied until the actual
//! work ends, so hung mounts cannot create an unbounded queue or freeze the reactor.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Why [`Pool::run`] produced no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// Every slot is held (possibly by workers stuck in the kernel): the job never ran. Not
    /// the job's fault, so callers must not treat it as a failure of what it would have done.
    Saturated,
    /// The deadline passed before the job started or finished.
    Expired,
    /// The worker panicked.
    Panicked,
}

#[derive(Clone)]
pub struct Pool {
    slots: Arc<Semaphore>,
}

impl Pool {
    pub fn new(workers: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(workers)),
        }
    }

    /// Takes every free slot until the permit drops, so tests can saturate the pool.
    #[cfg(test)]
    pub fn hold_all(&self) -> tokio::sync::OwnedSemaphorePermit {
        let free = u32::try_from(self.slots.available_permits()).expect("slot count fits in u32");
        Arc::clone(&self.slots)
            .try_acquire_many_owned(free)
            .expect("pool is open")
    }

    /// There is deliberately no admission queue: a full pool rejects at once. Dropping this
    /// future cannot release a running worker's permit: the worker owns it, not the waiter.
    pub async fn run<T: Send + 'static>(
        &self,
        deadline: Instant,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Rejected> {
        if Instant::now() >= deadline {
            return Err(Rejected::Expired);
        }
        let permit = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| Rejected::Saturated)?;
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // Work admitted just before a deadline may not have started yet.
            (Instant::now() < deadline).then(job)
        });
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .map_err(|_| Rejected::Expired)?
            .map_err(|_| Rejected::Panicked)?;
        // timeout_at polls ready work before its timer; never accept a late result.
        if Instant::now() >= deadline {
            return Err(Rejected::Expired);
        }
        result.ok_or(Rejected::Expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    const WORKERS: usize = 4;

    struct DropSignal(tokio::sync::mpsc::UnboundedSender<()>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[tokio::test]
    async fn timed_out_workers_keep_slots_and_do_not_block_the_reactor() {
        let pool = Arc::new(Pool::new(WORKERS));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut jobs = Vec::new();
        for _ in 0..WORKERS {
            let pool = Arc::clone(&pool);
            let gate = Arc::clone(&gate);
            let started_tx = started_tx.clone();
            let dropped_tx = dropped_tx.clone();
            jobs.push(tokio::spawn(async move {
                pool.run(Instant::now() + Duration::from_secs(1), move || {
                    let (lock, cv) = &*gate;
                    let mut released = lock.lock().unwrap();
                    started_tx.send(()).unwrap();
                    while !*released {
                        released = cv.wait(released).unwrap();
                    }
                    drop(released);
                    DropSignal(dropped_tx)
                })
                .await
            }));
        }
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            for _ in 0..WORKERS {
                started_rx.recv().await.unwrap();
            }
        })
        .await;
        // Always release workers, even if an assertion fails, to avoid hanging the
        // test runtime's shutdown.
        let results = if started.is_ok() {
            let mut results = Vec::new();
            for job in jobs {
                results.push(job.await.unwrap());
            }
            let rejected = pool
                .run(Instant::now() + Duration::from_secs(1), || 42)
                .await;
            Some((results, rejected))
        } else {
            None
        };
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert!(started.is_ok(), "workers did not start");
        let (results, rejected) = results.unwrap();
        assert!(results.iter().all(|r| matches!(r, Err(Rejected::Expired))));
        assert_eq!(
            rejected,
            Err(Rejected::Saturated),
            "timeout must not free a running slot"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..WORKERS {
                dropped_rx.recv().await.unwrap();
            }
            while pool.slots.available_permits() != WORKERS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            pool.run(Instant::now() + Duration::from_secs(1), || 42)
                .await,
            Ok(42)
        );
    }

    #[tokio::test]
    async fn expired_jobs_are_not_admitted() {
        let pool = Pool::new(WORKERS);
        assert_eq!(
            pool.run::<()>(Instant::now(), || panic!("must not run"))
                .await,
            Err(Rejected::Expired)
        );
        assert_eq!(pool.slots.available_permits(), WORKERS);
    }

    #[tokio::test]
    async fn panicking_jobs_are_reported_and_free_their_slot() {
        let pool = Pool::new(1);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            pool.run::<()>(deadline, || panic!("boom")).await,
            Err(Rejected::Panicked)
        );
        assert_eq!(pool.run(deadline, || 7).await, Ok(7));
    }
}
