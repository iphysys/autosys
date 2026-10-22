use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use contracts::{
    Error, ErrorCode, ExecutionContext, Tool, ToolDescriptor, ToolOutput, ToolRequest,
};
use serde_json::json;

use crate::builders::id;

type Handler = dyn Fn(&ToolRequest) -> Result<ToolOutput, Error> + Send + Sync;

/// A tool whose behavior is a closure. Counts started calls, so tests can
/// prove a call was not repeated, and can be slowed down to test timeouts and
/// cancellation.
pub struct FakeTool {
    descriptor: ToolDescriptor,
    handler: Box<Handler>,
    delay: Option<Duration>,
    calls: AtomicUsize,
}

impl FakeTool {
    pub fn new(
        name: &str,
        handler: impl Fn(&ToolRequest) -> Result<ToolOutput, Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: id(name),
                version: "0.1.0".to_owned(),
                description: format!("Fake tool {name}."),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
            },
            handler: Box::new(handler),
            delay: None,
            calls: AtomicUsize::new(0),
        }
    }

    /// Returns `{"arguments": <the request's arguments>}`.
    pub fn echo(name: &str) -> Self {
        Self::new(name, |request| {
            Ok(ToolOutput {
                structured: json!({ "arguments": request.arguments }),
            })
        })
    }

    /// Always reports a tool failure.
    pub fn failing(name: &str, message: &str) -> Self {
        let message = message.to_owned();
        Self::new(name, move |_| {
            Err(Error::new(ErrorCode::ToolFailed, message.clone()))
        })
    }

    /// Waits `delay` before answering; cancellation interrupts the wait.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Calls that started, including ones later cancelled or timed out.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn invoke(
        &self,
        request: &ToolRequest,
        ctx: &ExecutionContext,
    ) -> Result<ToolOutput, Error> {
        ctx.ensure_not_cancelled()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = ctx.cancelled() => ctx.ensure_not_cancelled()?,
            }
        }
        (self.handler)(request)
    }
}
