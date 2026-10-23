//! The tool gateway: the harness side of every tool call (blueprint section
//! 14.2).
//!
//! - [`McpServer`] connects to one MCP server over streamable HTTP and turns
//!   each tool it offers into a [`contracts::Tool`].
//! - [`ToolRegistry`] keeps the discovered tools that a `config/tools/`
//!   overlay lists. Every other tool is unavailable.
//! - [`ToolGateway`] runs a call: replay from its step record, argument
//!   validation, gates, execution with a timeout and the context's
//!   cancellation, result normalization, evidence, and the step record
//!   (ADR-0026).
//!
//! A tool error, a timeout or an invalid result is returned as
//! [`ToolObservation::Failed`], never as success.

mod gateway;
mod mcp;
mod normalizer;
mod registry;

pub use gateway::{ToolCall, ToolGateway, ToolObservation};
pub use mcp::{McpServer, McpSettings, McpTool, PROTOCOL_VERSION};
pub use registry::{ServerTools, ToolRegistry};

use std::future::Future;
use std::time::Duration;

use contracts::{Error, ErrorCode, ExecutionContext};

/// Runs `operation` until it finishes, `limit` passes, or `ctx` is
/// cancelled. Dropping the operation at the limit cancels it.
async fn bounded<T>(
    ctx: &ExecutionContext,
    limit: Duration,
    what: &str,
    operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::select! {
        biased;
        () = ctx.cancelled() => Err(ctx.ensure_not_cancelled().err().unwrap_or_else(|| {
            Error::new(ErrorCode::Cancelled, "operation cancelled")
        })),
        finished = tokio::time::timeout(limit, operation) => finished.unwrap_or_else(|_| {
            Err(Error::new(
                ErrorCode::Timeout,
                format!("{what} did not finish within {limit:?}"),
            ))
        }),
    }
}
