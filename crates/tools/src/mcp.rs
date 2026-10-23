//! The rmcp client: one connection per MCP server, over streamable HTTP.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{Identifier, ToolName};
use contracts::{
    Error, ErrorCode, ExecutionContext, Tool, ToolDescriptor, ToolOutput, ToolRequest,
};
use rmcp::model::{CallToolRequestParams, ContentBlock, ProtocolVersion};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::Value;

use crate::bounded;
use crate::registry::ServerTools;

/// The MCP revision the harness speaks: `server/discover` and per-request
/// metadata, no `initialize` handshake. A server that does not support it
/// fails at connect instead of being silently downgraded.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// Time limits on a server connection.
#[derive(Clone, Debug, PartialEq)]
pub struct McpSettings {
    /// Bounds connecting and listing the server's tools.
    pub connect_timeout: Duration,
    /// Bounds any single call. The gateway also applies each tool's own
    /// timeout from `config/tools/`.
    pub call_timeout: Duration,
}

type Client = RunningService<RoleClient, ()>;

/// A connected MCP server and the tools it offers.
pub struct McpServer {
    server_id: Identifier,
    tools: Vec<Arc<McpTool>>,
}

impl McpServer {
    /// Connects to the server at `url` (its MCP endpoint, for example
    /// `http://127.0.0.1:18123/mcp`) and lists its tools.
    ///
    /// Unavailable if the server cannot be reached or does not speak
    /// [`PROTOCOL_VERSION`]; Timeout after `connect_timeout`.
    pub async fn connect(
        server_id: Identifier,
        url: &str,
        settings: &McpSettings,
        ctx: &ExecutionContext,
    ) -> Result<Self, Error> {
        ctx.ensure_not_cancelled()?;
        let what = format!("connecting to tool server {}", server_id.as_str());
        let (client, listed) = bounded(ctx, settings.connect_timeout, &what, async {
            let transport = StreamableHttpClientTransport::from_uri(url.to_owned());
            let lifecycle = ClientLifecycleMode::Discover {
                preferred_versions: vec![PROTOCOL_VERSION],
            };
            let client =
                ().serve_with_lifecycle(transport, lifecycle)
                    .await
                    .map_err(|e| {
                        Error::new(
                            ErrorCode::Unavailable,
                            format!("cannot connect to tool server {}", server_id.as_str()),
                        )
                        .with_source(e)
                    })?;
            let listed = client
                .list_all_tools()
                .await
                .map_err(|e| service_error(&server_id, "list tools", e))?;
            Ok((client, listed))
        })
        .await?;

        let version = client
            .peer_info()
            .and_then(|info| info.server_info.as_ref().map(|s| s.version.clone()))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Unavailable,
                    format!(
                        "tool server {} did not report its version",
                        server_id.as_str()
                    ),
                )
            })?;
        let client = Arc::new(client);
        let tools = listed
            .into_iter()
            .map(|tool| {
                let name: ToolName = tool.name.parse().map_err(|_| {
                    Error::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "tool server {} offers a tool with an invalid name: {}",
                            server_id.as_str(),
                            tool.name
                        ),
                    )
                })?;
                Ok(Arc::new(McpTool {
                    server_id: server_id.clone(),
                    descriptor: ToolDescriptor {
                        name,
                        version: version.clone(),
                        description: tool.description.map(|d| d.into_owned()).unwrap_or_default(),
                        input_schema: Value::Object((*tool.input_schema).clone()),
                        output_schema: tool.output_schema.map(|s| Value::Object((*s).clone())),
                    },
                    client: Arc::clone(&client),
                    call_timeout: settings.call_timeout,
                }))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        tracing::info!(
            server = server_id.as_str(),
            %version,
            tools = tools.len(),
            "connected to tool server"
        );
        Ok(Self { server_id, tools })
    }

    pub fn server_id(&self) -> &Identifier {
        &self.server_id
    }

    pub fn tools(&self) -> &[Arc<McpTool>] {
        &self.tools
    }

    /// The server's tools, for [`crate::ToolRegistry::new`].
    pub fn into_server_tools(self) -> ServerTools {
        ServerTools {
            server_id: self.server_id,
            tools: self.tools.into_iter().map(|t| t as Arc<dyn Tool>).collect(),
        }
    }
}

/// One tool of a connected server.
pub struct McpTool {
    server_id: Identifier,
    descriptor: ToolDescriptor,
    client: Arc<Client>,
    call_timeout: Duration,
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn invoke(
        &self,
        request: &ToolRequest,
        ctx: &ExecutionContext,
    ) -> Result<ToolOutput, Error> {
        ctx.ensure_not_cancelled()?;
        let tool = request.tool.as_str();
        let params =
            CallToolRequestParams::new(tool.to_owned()).with_arguments(request.arguments.clone());
        let what = format!("tool {tool}");
        let result = bounded(ctx, self.call_timeout, &what, async {
            self.client
                .call_tool(params)
                .await
                .map_err(|e| service_error(&self.server_id, tool, e))
        })
        .await?;
        if result.is_error == Some(true) {
            return Err(Error::new(
                ErrorCode::ToolFailed,
                format!("{tool} failed: {}", reported_error(&result.content)),
            ));
        }
        result
            .structured_content
            .map(|structured| ToolOutput { structured })
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::ToolFailed,
                    format!("{tool} returned no structured result"),
                )
            })
    }
}

/// The error a tool reported. Tool servers send `{code, message, retryable}`
/// as JSON text; anything else is passed on as it is.
fn reported_error(content: &[ContentBlock]) -> String {
    let text: Vec<&str> = content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect();
    let joined = text.join(" ");
    match serde_json::from_str::<Value>(&joined) {
        Ok(Value::Object(error)) => match (error.get("code"), error.get("message")) {
            (Some(Value::String(code)), Some(Value::String(message))) => {
                format!("{code}: {message}")
            }
            _ => joined,
        },
        _ if joined.is_empty() => "no error details".to_owned(),
        _ => joined,
    }
}

fn service_error(server: &Identifier, op: &str, err: ServiceError) -> Error {
    let code = match &err {
        // The server answered with a JSON-RPC error, for example invalid
        // arguments or an unknown tool: it refused the call.
        ServiceError::McpError(_) => ErrorCode::ToolFailed,
        ServiceError::TransportSend(_) | ServiceError::TransportClosed => ErrorCode::Unavailable,
        ServiceError::Timeout { .. } => ErrorCode::Timeout,
        ServiceError::Cancelled { .. } => ErrorCode::Cancelled,
        _ => ErrorCode::Internal,
    };
    let message = format!("tool server {}: {op}: {err}", server.as_str());
    Error::new(code, message).with_source(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_errors_keep_the_servers_code_and_message() {
        let content = vec![ContentBlock::text(
            r#"{"code": "invalid_geometry", "message": "holes break the edge", "retryable": false}"#,
        )];
        assert_eq!(
            reported_error(&content),
            "invalid_geometry: holes break the edge"
        );
        assert_eq!(
            reported_error(&[ContentBlock::text("plain text")]),
            "plain text"
        );
        assert_eq!(reported_error(&[]), "no error details");
    }
}
