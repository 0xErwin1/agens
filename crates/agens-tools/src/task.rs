use agens_core::{AgentDefinition, AgentMode, Error};
use serde_json::Value;

use crate::{
    AgentCatalog, AgentModelValidator, DispatchTool, SkillCatalog, ToolExecutionContext, ToolOutput,
};

const MAX_TASK_DESCRIPTION_CHARS: usize = 16_384;
const MAX_TASK_MODEL_CHARS: usize = 64;
const MAX_TASK_SKILLS: usize = 128;
const MAX_TASK_SKILL_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInvocation {
    agent: Option<String>,
    model: Option<String>,
    skills: Option<Vec<String>>,
    description: String,
}

impl TaskInvocation {
    pub fn from_value(value: Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("task arguments must be an object")?;
        if object.len() > 4
            || object.keys().any(|key| {
                key != "agent" && key != "description" && key != "model" && key != "skills"
            })
        {
            return Err("task arguments are invalid".into());
        }

        let agent = match object.get("agent") {
            Some(Value::String(value)) if is_bounded_name(value, MAX_TASK_SKILL_NAME_CHARS) => {
                Some(value.clone())
            }
            Some(_) => return Err("task agent is invalid".into()),
            None => None,
        };
        let model = match object.get("model") {
            Some(Value::String(value)) if is_bounded_name(value, MAX_TASK_MODEL_CHARS) => {
                Some(value.clone())
            }
            Some(_) => return Err("task model is invalid".into()),
            None => None,
        };
        let skills = match object.get("skills") {
            Some(Value::Array(values))
                if values.len() <= MAX_TASK_SKILLS
                    && values.iter().all(|value| {
                        value
                            .as_str()
                            .is_some_and(|name| is_bounded_name(name, MAX_TASK_SKILL_NAME_CHARS))
                    })
                    && values
                        .iter()
                        .map(|value| value.as_str().expect("validated task skill"))
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == values.len() =>
            {
                Some(
                    values
                        .iter()
                        .map(|value| value.as_str().expect("validated task skill").to_owned())
                        .collect(),
                )
            }
            Some(_) => return Err("task skills are invalid".into()),
            None => None,
        };
        let description = object
            .get("description")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty() && value.chars().count() <= MAX_TASK_DESCRIPTION_CHARS
            })
            .ok_or("task description is invalid")?
            .to_owned();

        Ok(Self {
            agent,
            model,
            skills,
            description,
        })
    }
}

fn is_bounded_name(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.chars().count() <= limit
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSkill {
    name: String,
    description: String,
    instructions: String,
}

impl TaskSkill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTurnRequest {
    agent_name: String,
    agent_description: String,
    system_prompt: String,
    model: String,
    skills: Vec<TaskSkill>,
    description: String,
}

impl TaskTurnRequest {
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn agent_description(&self) -> &str {
        &self.agent_description
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn skills(&self) -> &[TaskSkill] {
        &self.skills
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

pub trait TaskRunner: Send {
    fn run(&mut self, request: TaskTurnRequest) -> Result<ToolOutput, Error>;
}

pub struct TaskTool<R> {
    agents: AgentCatalog,
    skills: SkillCatalog,
    parent_model: String,
    model_validator: Box<dyn AgentModelValidator + Send>,
    runner: R,
}

impl<R> TaskTool<R> {
    pub fn from_catalogs_with_model_validator(
        agents: AgentCatalog,
        skills: SkillCatalog,
        parent_model: impl Into<String>,
        model_validator: impl AgentModelValidator + Send + 'static,
        runner: R,
    ) -> Self {
        Self {
            agents,
            skills,
            parent_model: parent_model.into(),
            model_validator: Box::new(model_validator),
            runner,
        }
    }

    pub fn input_schema() -> Value {
        serde_json::json!({"type":"object","additionalProperties":false,"required":["description"],"properties":{"agent":{"type":"string","minLength":1,"maxLength":64},"description":{"type":"string","minLength":1,"maxLength":16384},"model":{"type":"string","minLength":1,"maxLength":64},"skills":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}}}})
    }

    fn resolve_agent(&self, requested: Option<&str>) -> Result<&AgentDefinition, ToolOutput> {
        requested
            .and_then(|name| self.agents.agent(name))
            .or_else(|| {
                requested
                    .is_none()
                    .then(|| {
                        self.agents
                            .subagents()
                            .filter(|agent| agent.mode == AgentMode::Subagent)
                            .min_by(|left, right| left.name.cmp(&right.name))
                    })
                    .flatten()
            })
            .filter(|agent| agent.mode == AgentMode::Subagent)
            .ok_or_else(|| ToolOutput::failure("task: requested agent is unavailable"))
    }

    fn resolve(&self, invocation: TaskInvocation) -> Result<TaskTurnRequest, ToolOutput> {
        let agent = self.resolve_agent(invocation.agent.as_deref())?;

        let model = invocation
            .model
            .or_else(|| agent.model.clone())
            .unwrap_or_else(|| self.parent_model.clone());
        if self.model_validator.validate_model(&model).is_err() {
            return Err(ToolOutput::failure("task: requested model is unavailable"));
        }

        let skills = self.resolve_skills(agent, invocation.skills.as_deref())?;
        Ok(TaskTurnRequest {
            agent_name: agent.name.clone(),
            agent_description: agent.description.clone(),
            system_prompt: agent.system_prompt.clone(),
            model,
            skills,
            description: invocation.description,
        })
    }

    fn resolve_skills(
        &self,
        agent: &AgentDefinition,
        requested: Option<&[String]>,
    ) -> Result<Vec<TaskSkill>, ToolOutput> {
        let names = requested.unwrap_or(&agent.skills);
        if !names.iter().all(|name| agent.skills.contains(name)) {
            return Err(ToolOutput::failure("task: requested skill is unavailable"));
        }

        names
            .iter()
            .map(|name| {
                let skill = self
                    .skills
                    .skill(name)
                    .ok_or_else(|| ToolOutput::failure("task: requested skill is unavailable"))?;
                let instructions = skill
                    .load_instructions()
                    .map_err(|_| ToolOutput::failure("task: requested skill is unavailable"))?;
                Ok(TaskSkill {
                    name: skill.name().to_owned(),
                    description: skill.description().to_owned(),
                    instructions,
                })
            })
            .collect()
    }
}

impl<R: TaskRunner> DispatchTool for TaskTool<R> {
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        let invocation = TaskInvocation::from_value(arguments.clone())
            .map_err(|_| Error::Tool("task arguments are invalid".into()))?;
        self.resolve_agent(invocation.agent.as_deref())
            .map(|agent| agent.name.clone())
            .map_err(|_| Error::Tool("task: requested agent is unavailable".into()))
    }

    fn execute(&mut self, _: &ToolExecutionContext, arguments: Value) -> Result<ToolOutput, Error> {
        let invocation = match TaskInvocation::from_value(arguments) {
            Ok(invocation) => invocation,
            Err(_) => return Ok(ToolOutput::failure("task: input exceeds configured bounds")),
        };
        let request = match self.resolve(invocation) {
            Ok(request) => request,
            Err(output) => return Ok(output),
        };

        self.runner.run(request)
    }
}
