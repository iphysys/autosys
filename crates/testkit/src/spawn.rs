//! A [`Spawner`] a test controls: it can hold a background task back, for
//! example a worker's heartbeat so that its lease runs out, and start it
//! later or never.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use contracts::{BackgroundTask, Spawner, TaskHandle};
use tokio::task::AbortHandle;

struct Entry {
    name: &'static str,
    held: Option<BackgroundTask>,
    running: Option<AbortHandle>,
}

#[derive(Default)]
struct State {
    hold: bool,
    next: u64,
    tasks: BTreeMap<u64, Entry>,
}

/// Holds every task it is given until [`ManualSpawner::release`], or, built
/// with [`ManualSpawner::running`], starts them on the tokio runtime at
/// once. Dropping a task's handle stops it either way. Cloning shares the
/// tasks.
#[derive(Clone)]
pub struct ManualSpawner {
    state: Arc<Mutex<State>>,
}

impl ManualSpawner {
    /// Holds every task back.
    pub fn holding() -> Self {
        Self::with_hold(true)
    }

    /// Starts every task at once, like a real spawner.
    pub fn running() -> Self {
        Self::with_hold(false)
    }

    fn with_hold(hold: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                hold,
                ..State::default()
            })),
        }
    }

    /// The names of the tasks held back, in the order they were given.
    pub fn held(&self) -> Vec<&'static str> {
        let state = self.state.lock().unwrap();
        state
            .tasks
            .values()
            .filter(|e| e.held.is_some())
            .map(|e| e.name)
            .collect()
    }

    /// The names of the tasks started and not stopped.
    pub fn started(&self) -> Vec<&'static str> {
        let state = self.state.lock().unwrap();
        state
            .tasks
            .values()
            .filter(|e| e.running.as_ref().is_some_and(|r| !r.is_finished()))
            .map(|e| e.name)
            .collect()
    }

    /// Starts the held tasks named `name`, and returns how many.
    pub fn release(&self, name: &str) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut started = 0;
        for entry in state.tasks.values_mut().filter(|e| e.name == name) {
            if let Some(task) = entry.held.take() {
                entry.running = Some(tokio::spawn(task).abort_handle());
                started += 1;
            }
        }
        started
    }
}

impl Spawner for ManualSpawner {
    fn spawn(&self, name: &'static str, task: BackgroundTask) -> TaskHandle {
        let mut state = self.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        let (held, running) = if state.hold {
            (Some(task), None)
        } else {
            (None, Some(tokio::spawn(task).abort_handle()))
        };
        state.tasks.insert(
            id,
            Entry {
                name,
                held,
                running,
            },
        );
        let shared = Arc::clone(&self.state);
        TaskHandle::new(move || {
            if let Some(entry) = shared.lock().unwrap().tasks.remove(&id)
                && let Some(running) = entry.running
            {
                running.abort();
            }
        })
    }
}
