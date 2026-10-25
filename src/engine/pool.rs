//! A fixed-size pool of OS threads for rule evaluation.
//!
//! Rule evaluation is CPU-bound and synchronous, so it never runs on the
//! async I/O threads. The pool size is the `worker_threads` setting; the
//! number of admitted checks is bounded separately by the execution manager,
//! which bounds the queue.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use serde::Serialize;
use tokio::sync::oneshot;

type Task = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    #[error("the worker pool is shut down")]
    Closed,
    #[error("a worker panicked while running the task")]
    Panicked,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolStats {
    pub threads: usize,
    pub busy: usize,
    pub queued: usize,
}

struct Counters {
    busy: AtomicUsize,
    queued: AtomicUsize,
}

pub struct WorkerPool {
    sender: Mutex<Option<mpsc::Sender<Task>>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
    threads: usize,
    counters: Arc<Counters>,
}

impl WorkerPool {
    pub fn new(threads: usize) -> Arc<Self> {
        let threads = threads.max(1);
        let (sender, receiver) = mpsc::channel::<Task>();
        let receiver = Arc::new(Mutex::new(receiver));
        let counters = Arc::new(Counters {
            busy: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
        });
        let handles = (0..threads)
            .map(|i| {
                let receiver = receiver.clone();
                let counters = counters.clone();
                std::thread::Builder::new()
                    .name(format!("drover-worker-{i}"))
                    .spawn(move || worker_loop(&receiver, &counters))
                    .expect("failed to spawn worker thread")
            })
            .collect();
        Arc::new(WorkerPool {
            sender: Mutex::new(Some(sender)),
            handles: Mutex::new(handles),
            threads,
            counters,
        })
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            threads: self.threads,
            busy: self.counters.busy.load(Ordering::Relaxed),
            queued: self.counters.queued.load(Ordering::Relaxed),
        }
    }

    /// Queues a task. Panics inside the task are contained by the worker.
    pub fn execute(&self, task: impl FnOnce() + Send + 'static) -> Result<(), PoolError> {
        let sender = self.sender.lock().unwrap();
        let sender = sender.as_ref().ok_or(PoolError::Closed)?;
        self.counters.queued.fetch_add(1, Ordering::Relaxed);
        sender.send(Box::new(task)).map_err(|_| {
            self.counters.queued.fetch_sub(1, Ordering::Relaxed);
            PoolError::Closed
        })
    }

    /// Runs `f` on the pool and awaits its result.
    pub async fn spawn<R: Send + 'static>(
        &self,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, PoolError> {
        let (tx, rx) = oneshot::channel();
        self.execute(move || {
            let _ = tx.send(f());
        })?;
        // The sender is dropped without sending only if `f` panicked.
        rx.await.map_err(|_| PoolError::Panicked)
    }

    /// Stops accepting tasks, lets queued tasks finish and joins the threads.
    pub fn shutdown(&self) {
        self.sender.lock().unwrap().take();
        let handles = std::mem::take(&mut *self.handles.lock().unwrap());
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.sender.get_mut().unwrap().take();
    }
}

fn worker_loop(receiver: &Mutex<mpsc::Receiver<Task>>, counters: &Counters) {
    loop {
        let task = {
            let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
            receiver.recv()
        };
        let Ok(task) = task else {
            return;
        };
        counters.queued.fetch_sub(1, Ordering::Relaxed);
        counters.busy.fetch_add(1, Ordering::Relaxed);
        if catch_unwind(AssertUnwindSafe(task)).is_err() {
            tracing::error!("a worker task panicked");
        }
        counters.busy.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_tasks_and_contains_panics() {
        let pool = WorkerPool::new(2);
        assert_eq!(pool.spawn(|| 40 + 2).await, Ok(42));
        let r: Result<(), _> = pool.spawn(|| panic!("boom")).await;
        assert_eq!(r, Err(PoolError::Panicked));
        // Workers survive panics.
        assert_eq!(pool.spawn(|| 1).await, Ok(1));
        pool.shutdown();
        assert_eq!(pool.spawn(|| 1).await, Err(PoolError::Closed));
    }

    #[tokio::test]
    async fn bounded_parallelism() {
        let pool = WorkerPool::new(3);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut waits = Vec::new();
        for _ in 0..12 {
            let (active, peak) = (active.clone(), peak.clone());
            waits.push(pool.spawn(move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for w in waits {
            w.await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }
}
