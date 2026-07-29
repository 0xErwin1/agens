use std::collections::BTreeMap;

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
    inherited_models: BTreeMap<ProfileScope, ProfileEditorValue<String>>,
    inherited_efforts: BTreeMap<ProfileScope, ProfileEditorValue<Option<String>>>,
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
        let model = model.into();
        let model = ProfileEditorValue {
            value: model,
            origin: model_origin,
        };
        let effort = ProfileEditorValue {
            value: effort.map(ToOwned::to_owned),
            origin: effort_origin,
        };
        Self {
            name: name.into(),
            inherited_models: BTreeMap::from([
                (ProfileScope::Global, model.clone()),
                (ProfileScope::Project, model.clone()),
            ]),
            inherited_efforts: BTreeMap::from([
                (ProfileScope::Global, effort.clone()),
                (ProfileScope::Project, effort.clone()),
            ]),
            model,
            effort,
            unavailable,
        }
    }

    pub fn with_scope_inherited_values(
        mut self,
        scope: ProfileScope,
        model: ProfileEditorValue<String>,
        effort: ProfileEditorValue<Option<String>>,
    ) -> Self {
        self.inherited_models.insert(scope, model);
        self.inherited_efforts.insert(scope, effort);
        self
    }
}

#[derive(Clone, Debug)]
pub struct ProfileEditor {
    original: Vec<ProfileEditorRow>,
    rows: Vec<ProfileEditorRow>,
    scope: ProfileScope,
    patches: BTreeMap<ProfileScope, BTreeMap<String, AgentProfilePatch>>,
}

impl ProfileEditor {
    pub fn new(rows: Vec<ProfileEditorRow>) -> Self {
        Self {
            original: rows.clone(),
            rows,
            scope: ProfileScope::Global,
            patches: BTreeMap::new(),
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
    pub fn patches(&self) -> Vec<AgentProfilePatch> {
        self.patches_for(self.scope)
            .map(|(_, patch)| patch.clone())
            .collect()
    }

    pub fn patches_for(
        &self,
        scope: ProfileScope,
    ) -> impl Iterator<Item = (&str, &AgentProfilePatch)> {
        self.patches
            .get(&scope)
            .into_iter()
            .flat_map(|patches| patches.iter())
            .map(|(name, patch)| (name.as_str(), patch))
    }

    pub fn named_patches(&self) -> impl Iterator<Item = (&str, &AgentProfilePatch)> {
        self.patches_for(self.scope)
    }

    pub fn set_model(&mut self, name: &str, model: impl Into<String>) {
        let origin = scope_origin(self.scope);
        if let Some(row) = self.row_mut(name) {
            row.model = ProfileEditorValue {
                value: model.into(),
                origin,
            };
            self.patch_mut(name).model = Some(Some(row.model.value.clone()));
        }
    }

    pub fn set_effort(&mut self, name: &str, effort: impl Into<String>) {
        let origin = scope_origin(self.scope);
        if let Some(row) = self.row_mut(name) {
            row.effort = ProfileEditorValue {
                value: Some(effort.into()),
                origin,
            };
            self.patch_mut(name).effort = Some(row.effort.value.clone());
        }
    }

    pub fn reset_model(&mut self, name: &str) {
        let inherited = self
            .original
            .iter()
            .find(|row| row.name == name)
            .and_then(|row| row.inherited_models.get(&self.scope))
            .cloned();
        if let (Some(row), Some(value)) = (self.row_mut(name), inherited) {
            row.model = value;
            self.patch_mut(name).model = Some(None);
        }
    }

    pub fn reset_effort(&mut self, name: &str) {
        let inherited = self
            .original
            .iter()
            .find(|row| row.name == name)
            .and_then(|row| row.inherited_efforts.get(&self.scope))
            .cloned();
        if let (Some(row), Some(value)) = (self.row_mut(name), inherited) {
            row.effort = value;
            self.patch_mut(name).effort = Some(None);
        }
    }

    pub fn cancel(&mut self) {
        self.rows.clone_from(&self.original);
        self.patches.clear();
    }

    fn row_mut(&mut self, name: &str) -> Option<&mut ProfileEditorRow> {
        self.rows.iter_mut().find(|row| row.name == name)
    }

    fn patch_mut(&mut self, name: &str) -> &mut AgentProfilePatch {
        self.patches
            .entry(self.scope)
            .or_default()
            .entry(name.to_owned())
            .or_default()
    }
}

const fn scope_origin(scope: ProfileScope) -> ProfileOrigin {
    match scope {
        ProfileScope::Global => ProfileOrigin::GlobalProfile,
        ProfileScope::Project => ProfileOrigin::ProjectProfile,
    }
}
