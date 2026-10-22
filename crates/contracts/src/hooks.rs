//! Hooks observe lifecycle events; they never enforce (blueprint section 15.1).

use async_trait::async_trait;
use schema_types::Event;

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::Identifier;

#[async_trait]
pub trait EventSubscriber: Send + Sync {
    fn id(&self) -> &Identifier;

    /// Observes an appended event. The caller logs an error and carries on; a
    /// subscriber can never change or stop the run.
    async fn on_event(&self, event: &Event, ctx: &ExecutionContext) -> Result<(), Error>;
}
