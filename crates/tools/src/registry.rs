//! The tools the harness may call: discovered from the servers and listed in
//! a `config/tools/` overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use contracts::ids::{Identifier, ToolName};
use contracts::types::{ToolOverlay, ToolPolicy};
use contracts::{Error, ErrorCode, SchemaSet, Tool, ToolDescriptor};

/// The tools one server offers, as discovered.
pub struct ServerTools {
    pub server_id: Identifier,
    pub tools: Vec<Arc<dyn Tool>>,
}

pub(crate) struct Registered {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) policy: ToolPolicy,
    pub(crate) request_schema: String,
    pub(crate) result_schema: String,
}

/// Every available tool with its governance policy, and the canonical
/// schemas its requests and results are validated against.
pub struct ToolRegistry {
    tools: BTreeMap<String, Registered>,
    unavailable: Vec<ToolName>,
    schemas: SchemaSet,
}

impl ToolRegistry {
    /// A tool is available only if its server offers it **and** that
    /// server's overlay lists it. A discovered tool without an overlay entry
    /// is unavailable.
    ///
    /// InvalidInput, so startup fails, when: two servers share an id or a
    /// tool name; an overlay names no connected server, lists a tool twice,
    /// or lists a tool its server does not offer; or a listed tool has no
    /// `request_schema` or `result_schema`.
    pub fn new(overlays: &[ToolOverlay], servers: Vec<ServerTools>) -> Result<Self, Error> {
        let mut by_server: BTreeMap<String, BTreeMap<String, Arc<dyn Tool>>> = BTreeMap::new();
        let mut seen_tools = BTreeSet::new();
        for server in servers {
            let id = server.server_id.as_str().to_owned();
            if by_server.contains_key(&id) {
                return Err(invalid(format!("tool server {id} is connected twice")));
            }
            let mut tools = BTreeMap::new();
            for tool in server.tools {
                let name = tool.descriptor().name.as_str().to_owned();
                if !seen_tools.insert(name.clone()) {
                    return Err(invalid(format!(
                        "tool {name} is offered by more than one server"
                    )));
                }
                tools.insert(name, tool);
            }
            by_server.insert(id, tools);
        }

        let mut registered = BTreeMap::new();
        let mut schema_paths = BTreeSet::new();
        for overlay in overlays {
            let server = overlay.server_id.as_str();
            let offered = by_server.get_mut(server).ok_or_else(|| {
                invalid(format!(
                    "config/tools/ has an overlay for server {server}, which is not connected"
                ))
            })?;
            for policy in &overlay.tools {
                let name = policy.name.as_str();
                if registered.contains_key(name) {
                    return Err(invalid(format!(
                        "tool {name} is listed twice in config/tools/"
                    )));
                }
                let tool = offered.remove(name).ok_or_else(|| {
                    invalid(format!(
                        "config/tools/ lists {name}, which server {server} does not offer"
                    ))
                })?;
                let (Some(request_schema), Some(result_schema)) =
                    (&policy.request_schema, &policy.result_schema)
                else {
                    return Err(invalid(format!(
                        "tool {name} needs request_schema and result_schema in config/tools/"
                    )));
                };
                schema_paths.insert(request_schema.as_str().to_owned());
                schema_paths.insert(result_schema.as_str().to_owned());
                registered.insert(
                    name.to_owned(),
                    Registered {
                        tool,
                        policy: policy.clone(),
                        request_schema: request_schema.as_str().to_owned(),
                        result_schema: result_schema.as_str().to_owned(),
                    },
                );
            }
        }

        let unavailable: Vec<ToolName> = by_server
            .into_values()
            .flat_map(|tools| tools.into_values())
            .map(|tool| tool.descriptor().name.clone())
            .collect();
        for name in &unavailable {
            tracing::info!(
                tool = name.as_str(),
                "tool is unavailable: no config/tools/ entry"
            );
        }
        let paths: Vec<&str> = schema_paths.iter().map(String::as_str).collect();
        Ok(Self {
            tools: registered,
            unavailable,
            schemas: SchemaSet::compile(&paths)?,
        })
    }

    /// Names of the available tools, sorted.
    pub fn available(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Discovered tools that no overlay lists.
    pub fn unavailable(&self) -> &[ToolName] {
        &self.unavailable
    }

    pub fn descriptor(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name).map(|r| r.tool.descriptor())
    }

    pub fn policy(&self, name: &str) -> Option<&ToolPolicy> {
        self.tools.get(name).map(|r| &r.policy)
    }

    /// NotFound, naming the available tools, if `name` is unavailable.
    pub(crate) fn get(&self, name: &str) -> Result<&Registered, Error> {
        self.tools.get(name).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!(
                    "tool {name} is not available; available: {}",
                    self.available().join(", ")
                ),
            )
        })
    }

    pub(crate) fn schemas(&self) -> &SchemaSet {
        &self.schemas
    }
}

fn invalid(message: String) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}
