//! The one crossing point from the daemon's runtime into synchronous code.
//!
//! SQLite, tools and providers are synchronous and stay that way: the decision
//! for the daemon is to run them on the blocking pool rather than migrate them.
//! That only holds if there is a single, named way across. Calling synchronous
//! code directly from a runtime task stalls the worker thread it lands on, and
//! with it every timer and ingest task sharing that thread — a failure that is
//! invisible until the daemon is under load and painful to attribute afterwards.

use tokio::runtime::Handle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingError {
    /// The synchronous work panicked. Reported rather than propagated: one
    /// misbehaving subsystem must not take the machine's daemon down with it.
    Panicked,
    /// The runtime is going away, so the work will not run.
    ShuttingDown,
}

#[derive(Clone, Debug)]
pub struct BlockingBoundary {
    handle: Handle,
}

impl BlockingBoundary {
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// Runs `work` on the blocking pool and awaits its result, leaving the
    /// runtime's worker threads free.
    pub async fn run<T, F>(&self, work: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.handle.spawn_blocking(work).await.map_err(|error| {
            if error.is_panic() {
                BlockingError::Panicked
            } else {
                BlockingError::ShuttingDown
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// One worker thread on purpose: with several, work that wrongly ran inline
    /// could still leave another worker free and the stall would not show.
    fn single_worker_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap()
    }

    /// The boundary call has to be awaited from a *spawned task*, not from
    /// `block_on`: `block_on` drives its future on the calling thread, so work
    /// that wrongly ran inline would block that thread and leave the worker free
    /// to tick anyway. Spawned, the two tasks contend for the single worker and
    /// running inline starves the ticker.
    #[test]
    fn the_runtime_keeps_making_progress_while_blocking_work_waits_on_it() {
        let runtime = single_worker_runtime();
        let boundary = BlockingBoundary::new(runtime.handle().clone());
        let (sender, receiver) = mpsc::channel();

        let outcome = runtime.block_on(async move {
            let ticker = tokio::spawn(async move {
                for tick in 0..4 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    if sender.send(tick).is_err() {
                        return;
                    }
                }
            });

            let waiter = tokio::spawn(async move {
                boundary
                    .run(move || {
                        for _ in 0..4 {
                            receiver.recv_timeout(Duration::from_secs(5))?;
                        }
                        Ok::<_, mpsc::RecvTimeoutError>(())
                    })
                    .await
            });

            let outcome = waiter.await.unwrap();
            ticker.abort();
            outcome
        });

        assert_eq!(outcome, Ok(Ok(())));
    }

    #[test]
    fn a_panic_in_synchronous_work_is_reported_and_the_daemon_keeps_serving() {
        let runtime = single_worker_runtime();
        let boundary = BlockingBoundary::new(runtime.handle().clone());

        let panicked = runtime.block_on(boundary.run(|| panic!("a subsystem gave up")));
        assert_eq!(panicked, Err(BlockingError::Panicked));

        let after = runtime.block_on(boundary.run(|| "still serving"));
        assert_eq!(after, Ok("still serving"));
    }

    #[test]
    fn results_cross_the_boundary_intact() {
        let runtime = single_worker_runtime();
        let boundary = BlockingBoundary::new(runtime.handle().clone());

        let value = runtime.block_on(boundary.run(|| vec![1_u8, 2, 3]));

        assert_eq!(value, Ok(vec![1, 2, 3]));
    }
}
