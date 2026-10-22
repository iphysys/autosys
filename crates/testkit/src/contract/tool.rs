use contracts::{ErrorCode, Tool, ToolRequest};
use serde_json::json;

use super::{assert_code, cancelled_ctx, ctx};

/// The `Tool` contract. `valid_request` must be a request the tool accepts.
pub async fn tool(tool: &dyn Tool, valid_request: ToolRequest) {
    let descriptor = tool.descriptor();
    assert_eq!(
        descriptor.input_schema.get("type"),
        Some(&json!("object")),
        "tool {}: input schema is not an object schema",
        descriptor.name.as_str()
    );
    assert_eq!(
        valid_request.tool, descriptor.name,
        "tool suite precondition: the request names a different tool"
    );
    tool.invoke(&valid_request, &ctx())
        .await
        .unwrap_or_else(|e| {
            panic!(
                "tool {}: valid request failed: {e}",
                descriptor.name.as_str()
            )
        });
    assert_code(
        tool.invoke(&valid_request, &cancelled_ctx()).await,
        ErrorCode::Cancelled,
        "tool: cancelled context",
    );
}
