use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use contracts::{
    Error, ErrorCode, ExecutionContext, Tool, ToolDescriptor, ToolOutput, ToolRequest,
};
use serde_json::json;

use crate::builders::id;

type Handler = dyn Fn(&ToolRequest) -> Result<ToolOutput, Error> + Send + Sync;

/// What a [`FakeTool::before_answer`] hook returns: `Ok` to go on and
/// answer, an error to fail the call with it. It may wait first.
pub type HookFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

type Hook = dyn Fn(usize, &ToolRequest) -> HookFuture + Send + Sync;

/// A tool whose behavior is a closure. Counts started calls, so tests can
/// prove a call was not repeated, and can be slowed down to test timeouts and
/// cancellation. A hook can pause a call midway, for tests of what happens
/// meanwhile: a lease lost mid-call, a process killed mid-call.
pub struct FakeTool {
    descriptor: ToolDescriptor,
    handler: Box<Handler>,
    delay: Option<Duration>,
    hook: Option<Box<Hook>>,
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
            hook: None,
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

    /// Runs `hook` in every call, after the call is counted and before the
    /// delay and the answer. It gets the call's index, from 0, and its
    /// request; it may return at once, wait, or fail the call.
    pub fn before_answer(
        mut self,
        hook: impl Fn(usize, &ToolRequest) -> HookFuture + Send + Sync + 'static,
    ) -> Self {
        self.hook = Some(Box::new(hook));
        self
    }

    /// Calls that started, including ones later cancelled, timed out or
    /// failed by the hook.
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.hook {
            hook(index, request).await?;
        }
        if let Some(delay) = self.delay {
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = ctx.cancelled() => ctx.ensure_not_cancelled()?,
            }
        }
        (self.handler)(request)
    }
}

/// A value a hook needs that exists only after the hook is built, such as
/// the stores of the world the hooked tool is registered in. Fill it once,
/// after building; a hook that reads it earlier panics, naming the slot.
pub struct Slot<T> {
    name: &'static str,
    value: Arc<OnceLock<T>>,
}

impl<T> Slot<T> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            value: Arc::new(OnceLock::new()),
        }
    }

    /// Panics if the slot is already filled.
    pub fn fill(&self, value: T) {
        assert!(
            self.value.set(value).is_ok(),
            "slot {} was filled twice",
            self.name
        );
    }

    /// Panics if the slot is not filled yet.
    pub fn get(&self) -> &T {
        self.value.get().unwrap_or_else(|| {
            panic!(
                "slot {} was read before it was filled; fill it after building what it holds",
                self.name
            )
        })
    }

    /// The value, if the slot is filled.
    pub fn try_get(&self) -> Option<&T> {
        self.value.get()
    }
}

impl<T> Clone for Slot<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            value: Arc::clone(&self.value),
        }
    }
}
