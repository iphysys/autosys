//! The worker's [`Spawner`]: tokio tasks, tracked so a shutdown can wait for
//! every one of them.

use contracts::{BackgroundTask, Spawner, TaskHandle};
use tokio_util::task::TaskTracker;

/// Spawns on the current tokio runtime. Dropping a task's handle aborts it.
pub struct TokioSpawner {
    tracker: TaskTracker,
}

impl TokioSpawner {
    pub fn new(tracker: TaskTracker) -> Self {
        Self { tracker }
    }
}

impl Spawner for TokioSpawner {
    fn spawn(&self, name: &'static str, task: BackgroundTask) -> TaskHandle {
        tracing::debug!(task = name, "starting a background task");
        let handle = self.tracker.spawn(task);
        TaskHandle::new(move || handle.abort())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn dropping_the_handle_aborts_the_task_and_the_tracker_drains() {
        let tracker = TaskTracker::new();
        let spawner = TokioSpawner::new(tracker.clone());
        let handle = spawner.spawn(
            "forever",
            Box::pin(async { tokio::time::sleep(Duration::from_secs(3600)).await }),
        );
        drop(handle);
        tracker.close();
        tokio::time::timeout(Duration::from_secs(5), tracker.wait())
            .await
            .unwrap_or_else(|_| panic!("the aborted task still runs"));
    }
}
