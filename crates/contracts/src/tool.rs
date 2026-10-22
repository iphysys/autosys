//! The tool contract (blueprint section 4.1, item 3).
//!
//! Implemented by the MCP client adapter (PR 8) and by testkit's fake.
//! Arguments and results are JSON because each tool has its own shape; the
//! tool gateway validates them against the tool's schemas in `schemas/tools/`.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{Identifier, StepId, ToolName};

/// What a tool server says about one of its tools.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDescriptor {
    pub name: ToolName,
    pub version: String,
    pub description: String,
    /// The tool's input JSON Schema; always an object schema.
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolRequest {
    pub tool: ToolName,
    pub step_id: StepId,
    /// Recovery uses it to return a completed call's result instead of calling again.
    pub idempotency_key: Identifier,
    pub arguments: Map<String, Value>,
}

/// A successful tool result. Untrusted until validated against the tool's result schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub structured: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    /// Calls the tool. A failure the tool itself reports is
    /// `Err(ErrorCode::ToolFailed)`, never `Ok`. Returns `Cancelled` if `ctx`
    /// is cancelled.
    async fn invoke(
        &self,
        request: &ToolRequest,
        ctx: &ExecutionContext,
    ) -> Result<ToolOutput, Error>;
}
