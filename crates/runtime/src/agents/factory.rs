//! What a task may ask of its agent (blueprint sections 2.6 and 14.3), and
//! which model the agent runs on. The agent spec is the ceiling; a task can
//! narrow it, never widen it.

use contracts::ids::Identifier;
use contracts::types::{Budget, Task};
use model_gateway::ModelGateway;

use crate::agents::registry::{AgentDefinition, AgentRegistry, TASK_SCHEMA};
use crate::error::RuntimeError;

/// The model profile an agent runs on, and the most output a call with it
/// may ask for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelChoice {
    pub(crate) profile: Identifier,
    pub(crate) max_output_tokens: u64,
}

impl AgentDefinition {
    /// The profile the model gateway resolved for this agent at startup,
    /// where a profile without tool calling was already refused for an agent
    /// with tools (ADR-0027).
    pub(crate) fn model(&self, models: &ModelGateway) -> Result<ModelChoice, RuntimeError> {
        let profile = models
            .agent_profile(&self.spec().id)
            .map_err(|source| RuntimeError::Component {
                operation: "resolving the agent's model profile",
                source,
            })?
            .clone();
        let max_output_tokens = models
            .routing()
            .profile(profile.as_str())
            .map(|p| p.max_output_tokens.get())
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "model profile `{}` was resolved but is not in the routing",
                    profile.as_str()
                ))
            })?;
        Ok(ModelChoice {
            profile,
            max_output_tokens,
        })
    }
}

impl AgentRegistry {
    /// The agent `task` names, once the task has passed every check:
    /// - it satisfies `tasks/task.schema.json`, including what its Rust type
    ///   does not enforce;
    /// - its tools and skills are among the agent's;
    /// - each limit of its budget is at most the agent's.
    ///
    /// `UnknownAgent` if no such agent is registered; `InvalidTask` listing
    /// every other problem.
    pub fn agent_for(&self, task: &Task) -> Result<&AgentDefinition, RuntimeError> {
        let mut problems =
            self.task_schema
                .errors_of(TASK_SCHEMA, task)
                .map_err(|e| RuntimeError::Component {
                    operation: "validating the task",
                    source: e,
                })?;
        let agent = self.get(task.agent_id.as_str())?;
        let spec = agent.spec();
        let id = spec.id.as_str();

        let allowed: Vec<&str> = spec.tools.iter().map(|t| t.as_str()).collect();
        for tool in &task.tools {
            if !allowed.contains(&tool.as_str()) {
                problems.push(format!(
                    "tool `{}` is not one of agent `{id}`'s tools ({})",
                    tool.as_str(),
                    allowed.join(", ")
                ));
            }
        }
        for skill in &task.skills {
            if !spec.skills.contains(skill) {
                problems.push(format!(
                    "skill `{}` is not one of agent `{id}`'s skills",
                    skill.as_str()
                ));
            }
        }
        for (name, asked, limit) in limits(&task.budget, &spec.limits) {
            if asked > limit {
                problems.push(format!(
                    "budget {name} {asked} exceeds agent `{id}`'s limit {limit}"
                ));
            }
        }

        if problems.is_empty() {
            Ok(agent)
        } else {
            Err(RuntimeError::InvalidTask {
                task: task.id.as_str().to_owned(),
                problems,
            })
        }
    }
}

/// Each limit of `budget` beside the same limit of `ceiling`.
fn limits(budget: &Budget, ceiling: &Budget) -> [(&'static str, u64, u64); 5] {
    [
        (
            "max_tokens",
            budget.max_tokens.get(),
            ceiling.max_tokens.get(),
        ),
        (
            "max_tool_calls",
            budget.max_tool_calls.get(),
            ceiling.max_tool_calls.get(),
        ),
        ("max_steps", budget.max_steps.get(), ceiling.max_steps.get()),
        (
            "max_wall_time_seconds",
            budget.max_wall_time_seconds.get(),
            ceiling.max_wall_time_seconds.get(),
        ),
        (
            "max_repair_attempts",
            budget.max_repair_attempts.get(),
            ceiling.max_repair_attempts.get(),
        ),
    ]
}
