//! What a task may ask of its agent (blueprint sections 2.6 and 14.3). The
//! agent spec is the ceiling; a task can narrow it, never widen it.

use contracts::types::{Budget, Task};

use crate::agents::registry::{AgentDefinition, AgentRegistry, TASK_SCHEMA};
use crate::error::RuntimeError;

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
