use agens_agents::ProfileOrigin;
use agens_config::AgentProfilePatch;

pub trait AgentProfileStore: Send + Sync {
    fn save(
        &self,
        scope: ProfileScope,
        agent: &str,
        patch: &AgentProfilePatch,
    ) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProfileScope {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleDirection {
    Prev,
    Next,
}

pub const EFFORT_SEQUENCE: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileEditorValue<T> {
    pub value: T,
    pub origin: ProfileOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileEditorRow {
    pub name: String,
    pub model: ProfileEditorValue<String>,
    pub effort: ProfileEditorValue<Option<String>>,
    pub unavailable: bool,
}

impl ProfileEditorRow {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        model_origin: ProfileOrigin,
        effort: Option<&str>,
        effort_origin: ProfileOrigin,
        unavailable: bool,
    ) -> Self {
        Self {
            name: name.into(),
            model: ProfileEditorValue {
                value: model.into(),
                origin: model_origin,
            },
            effort: ProfileEditorValue {
                value: effort.map(ToOwned::to_owned),
                origin: effort_origin,
            },
            unavailable,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProfileEditor {
    rows: Vec<ProfileEditorRow>,
    scope: ProfileScope,
}

impl ProfileEditor {
    pub fn new(rows: Vec<ProfileEditorRow>) -> Self {
        Self {
            rows,
            scope: ProfileScope::Global,
        }
    }

    pub fn rows(&self) -> &[ProfileEditorRow] {
        &self.rows
    }

    pub const fn scope(&self) -> ProfileScope {
        self.scope
    }

    pub fn set_scope(&mut self, scope: ProfileScope) {
        self.scope = scope;
    }

    pub fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            ProfileScope::Global => ProfileScope::Project,
            ProfileScope::Project => ProfileScope::Global,
        };
    }

    pub fn effort_after(&self, name: &str, direction: CycleDirection) -> Option<Option<String>> {
        let effort = self
            .rows
            .iter()
            .find(|row| row.name == name)?
            .effort
            .value
            .as_deref();
        let index = match effort {
            None => match direction {
                CycleDirection::Prev => EFFORT_SEQUENCE.len() - 1,
                CycleDirection::Next => 0,
            },
            Some(effort) => {
                let index = EFFORT_SEQUENCE
                    .iter()
                    .position(|candidate| *candidate == effort)?;
                match direction {
                    CycleDirection::Prev => {
                        (index + EFFORT_SEQUENCE.len() - 1) % EFFORT_SEQUENCE.len()
                    }
                    CycleDirection::Next => (index + 1) % EFFORT_SEQUENCE.len(),
                }
            }
        };
        Some(Some(EFFORT_SEQUENCE[index].to_owned()))
    }
}
