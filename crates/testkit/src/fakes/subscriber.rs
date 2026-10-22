use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::Event;
use contracts::{Error, EventSubscriber, ExecutionContext};

use crate::builders::id;

/// Records every event it observes.
pub struct RecordingSubscriber {
    id: Identifier,
    events: Mutex<Vec<Event>>,
}

impl RecordingSubscriber {
    pub fn new() -> Self {
        Self {
            id: id("recording"),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl Default for RecordingSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventSubscriber for RecordingSubscriber {
    fn id(&self) -> &Identifier {
        &self.id
    }

    async fn on_event(&self, event: &Event, _ctx: &ExecutionContext) -> Result<(), Error> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}
