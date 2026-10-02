//! Bounded isolation for filesystem reads and extraction.
//!
//! A timed-out syscall cannot be cancelled. Its slot stays occupied until the actual
//! work ends, so hung mounts cannot create an unbounded queue or freeze the reactor.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::time::Instant;

const WORKERS: usize = 4;

#[derive(Clone)]
pub struct Pool {
    slots: Arc<Semaphore>,
}

impl Pool {
    pub fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(WORKERS)),
        }
    }

    /// Returns `None` on saturation, deadline expiry, or a panicking worker. There is
    /// deliberately no admission queue. Dropping this future cannot release a running
    /// worker's permit: the worker owns it, not the waiter.
    pub async fn run<T: Send + 'static>(
        &self,
        deadline: Instant,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        if Instant::now() >= deadline {
            return None;
        }
        let permit = Arc::clone(&self.slots).try_acquire_owned().ok()?;
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // Work admitted just before a deadline may not have started yet.
            (Instant::now() < deadline).then(job)
        });
        let result = tokio::time::timeout_at(deadline, work).await.ok()?.ok()?;
        // timeout_at polls ready work before its timer; never accept a late result.
        if Instant::now() >= deadline {
            return None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    struct DropSignal(tokio::sync::mpsc::UnboundedSender<()>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[tokio::test]
    async fn timed_out_workers_keep_slots_and_do_not_block_the_reactor() {
        let pool = Arc::new(Pool::new());
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
        assert!(results.iter().all(Option::is_none));
        assert_eq!(rejected, None, "timeout must not free a running slot");
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
            Some(42)
        );
    }

    #[tokio::test]
    async fn expired_jobs_are_not_admitted() {
        let pool = Pool::new();
        assert_eq!(
            pool.run::<()>(Instant::now(), || panic!("must not run"))
                .await,
            None
        );
        assert_eq!(pool.slots.available_permits(), WORKERS);
    }
}
