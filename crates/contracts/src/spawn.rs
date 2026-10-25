//! Background tasks, started by whoever holds a [`Spawner`]. A library that
//! needs one, such as the scheduler's heartbeat, receives a spawner from its
//! caller instead of reaching for a runtime itself, so tests can hold a task
//! back and bootstrap can track every task it started (ADR-0031).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// A task to run in the background.
pub type BackgroundTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub trait Spawner: Send + Sync {
    /// Starts `task`, named `name` for logs and tests. The task stops when the
    /// returned handle is dropped, if it has not finished by then.
    fn spawn(&self, name: &'static str, task: BackgroundTask) -> TaskHandle;
}

/// Owns a background task: dropping it stops the task.
pub struct TaskHandle {
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl TaskHandle {
    /// A handle that calls `stop` when dropped.
    pub fn new(stop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            stop: Some(Box::new(stop)),
        }
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

impl fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TaskHandle")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn dropping_the_handle_stops_the_task_once() {
        let stopped = Arc::new(AtomicBool::new(false));
        let handle = {
            let stopped = Arc::clone(&stopped);
            TaskHandle::new(move || {
                assert!(!stopped.swap(true, Ordering::SeqCst), "stopped twice");
            })
        };
        assert!(!stopped.load(Ordering::SeqCst));
        drop(handle);
        assert!(stopped.load(Ordering::SeqCst));
    }
}
