//! The agents the runtime can run (blueprint sections 13.1 and 14.3).
//!
//! Registration is explicit: bootstrap passes in the specs it loaded from
//! `config/agents/`, and `cargo xtask validate-config` builds the same
//! registry, so a spec that would fail at startup fails there first.

use std::collections::{BTreeMap, BTreeSet};

use context::PromptLibrary;
use contracts::types::{AgentSpec, AgentSpecOnRepairExhausted, Identifier, Sha256, VersionedHash};
use contracts::{Error, SchemaSet};
use serde::Serialize;
use sha2::Digest;

use crate::error::RuntimeError;
use crate::sdr_tools;

/// The schema every task is checked against before it runs.
pub(crate) const TASK_SCHEMA: &str = "tasks/task.schema.json";

/// One registered agent: its spec, the spec's provenance, and its compiled
/// result schema.
pub struct AgentDefinition {
    spec: AgentSpec,
    provenance: VersionedHash,
    result_schema: SchemaSet,
}

impl AgentDefinition {
    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    /// Id, version and sha256 of the spec, recorded with every commit the
    /// agent makes (blueprint section 9.5). The hash is over the spec's
    /// compact JSON, which serde writes in a fixed field order.
    pub fn provenance(&self) -> &VersionedHash {
        &self.provenance
    }

    /// Every way `value` breaks the agent's result schema; empty when valid.
    pub(crate) fn result_problems(&self, value: &impl Serialize) -> Result<Vec<String>, Error> {
        self.result_schema
            .errors_of(self.spec.result_schema.as_str(), value)
    }
}

/// The registered agents, by id.
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
    pub(crate) task_schema: SchemaSet,
}

impl AgentRegistry {
    /// Registers `specs`. `tools` are the names the tool gateway offers;
    /// `sdr_read` and `sdr_propose` are always available.
    ///
    /// Fails with every problem found (blueprint section 13.1), each naming
    /// what is available instead:
    /// - two specs with one id;
    /// - a prompt that is not in `prompts`;
    /// - any skill: there is no skill registry yet;
    /// - a tool that is not available;
    /// - a `result_schema` that does not compile;
    /// - `on_repair_exhausted: escalate`: every agent is top-level until
    ///   delegation arrives in Increment 3, so there is nobody to escalate to.
    pub fn new(
        specs: Vec<AgentSpec>,
        prompts: &PromptLibrary,
        tools: &[&str],
    ) -> Result<Self, RuntimeError> {
        let mut available: BTreeSet<&str> = tools.iter().copied().collect();
        available.extend(sdr_tools::NAMES);
        let available_list = available.iter().copied().collect::<Vec<_>>().join(", ");

        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        let mut agents = BTreeMap::new();
        for spec in specs {
            let id = spec.id.as_str().to_owned();
            if !seen.insert(id.clone()) {
                problems.push(format!("agent `{id}` is defined more than once"));
                continue;
            }
            let mut own = Vec::new();
            if let Err(e) = prompts.get(spec.prompt.as_str()) {
                own.push(format!("prompt: {e}"));
            }
            if !spec.skills.is_empty() {
                let skills: Vec<&str> = spec.skills.iter().map(|s| s.as_str()).collect();
                own.push(format!(
                    "skills {} are named, but there is no skill registry yet; available skills: none",
                    skills.join(", ")
                ));
            }
            for tool in &spec.tools {
                if !available.contains(tool.as_str()) {
                    own.push(format!(
                        "tool `{}` is not available; available: {available_list}",
                        tool.as_str()
                    ));
                }
            }
            if spec.on_repair_exhausted == AgentSpecOnRepairExhausted::Escalate {
                own.push(
                    "on_repair_exhausted: escalate needs an agent above this one, and \
                     delegation arrives in Increment 3; use fail"
                        .to_owned(),
                );
            }
            let result_schema = match SchemaSet::compile(&[spec.result_schema.as_str()]) {
                Ok(schema) => Some(schema),
                Err(e) => {
                    own.push(format!("result_schema: {}", e.message()));
                    None
                }
            };
            match result_schema {
                Some(result_schema) if own.is_empty() => {
                    let provenance = provenance(&spec)?;
                    agents.insert(
                        id,
                        AgentDefinition {
                            spec,
                            provenance,
                            result_schema,
                        },
                    );
                }
                _ => problems.extend(own.into_iter().map(|p| format!("agent `{id}`: {p}"))),
            }
        }
        if !problems.is_empty() {
            return Err(RuntimeError::InvalidAgents { problems });
        }
        let task_schema = SchemaSet::compile(&[TASK_SCHEMA]).map_err(|e| {
            RuntimeError::Internal(format!("the task schema does not compile: {e}"))
        })?;
        Ok(Self {
            agents,
            task_schema,
        })
    }

    /// The agent with id `id`. The error lists the registered agents.
    pub fn get(&self, id: &str) -> Result<&AgentDefinition, RuntimeError> {
        self.agents
            .get(id)
            .ok_or_else(|| RuntimeError::UnknownAgent {
                id: id.to_owned(),
                available: self.ids().map(str::to_owned).collect(),
            })
    }

    /// Registered agent ids, in order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.agents.keys().map(String::as_str)
    }

    /// Every registered spec, in id order: what the model gateway resolves
    /// profiles for at startup.
    pub fn specs(&self) -> Vec<AgentSpec> {
        self.agents.values().map(|a| a.spec.clone()).collect()
    }
}

fn provenance(spec: &AgentSpec) -> Result<VersionedHash, RuntimeError> {
    let json = serde_json::to_vec(spec)
        .map_err(|e| RuntimeError::Internal(format!("cannot serialize an agent spec: {e}")))?;
    let hex: String = sha2::Sha256::digest(&json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let invalid =
        |what: &str| RuntimeError::Internal(format!("the agent spec's {what} is invalid"));
    Ok(VersionedHash {
        id: spec
            .id
            .as_str()
            .parse::<Identifier>()
            .map_err(|_| invalid("id"))?,
        version: Some(spec.version.clone()),
        sha256: format!("sha256:{hex}")
            .parse::<Sha256>()
            .map_err(|_| invalid("hash"))?,
    })
}
