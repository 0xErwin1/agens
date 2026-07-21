use agens_core::{ReasoningEffort, RequestConfig};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SNAPSHOT: &[u8] = include_bytes!("../data/models.dev-openai.json");
const SNAPSHOT_CHECKSUM: &str = include_str!("../data/models.dev-openai.json.sha256");
const GPT_5_6_MODELS: [(&str, &str); 4] = [
    ("gpt-5.6", "GPT-5.6 (Sol alias)"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelMetadata {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) context: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) reasoning: Option<bool>,
    pub(crate) input_price: Option<f64>,
    pub(crate) output_price: Option<f64>,
}

#[derive(Debug)]
pub(crate) enum ModelRegistryError {
    Checksum,
    Schema,
}

/// The bundled snapshot's source and revision are recorded in its JSON metadata.
pub(crate) fn bundled_openai_models() -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    if bundled_snapshot_checksum() != SNAPSHOT_CHECKSUM.trim() {
        return Err(ModelRegistryError::Checksum);
    }

    parse_models(SNAPSHOT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiModelSource {
    OpenAiApi,
    ChatGptSubscription,
}

impl TuiModelSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenAiApi => "OpenAI API",
            Self::ChatGptSubscription => "ChatGPT subscription",
        }
    }
}

/// Validates and retains the bounded selections exposed by the terminal UI adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiModelSelector {
    model: String,
    source: TuiModelSource,
    metadata_known: bool,
    reasoning_effort: Option<ReasoningEffort>,
    request_config: RequestConfig,
}

impl TuiModelSelector {
    pub fn new(model: impl Into<String>) -> Self {
        Self::for_source(model, TuiModelSource::OpenAiApi)
    }

    pub fn for_source(model: impl Into<String>, source: TuiModelSource) -> Self {
        Self {
            model: model.into(),
            source,
            metadata_known: true,
            reasoning_effort: None,
            request_config: RequestConfig::default(),
        }
    }

    pub fn model_values(&self) -> Result<Vec<String>, String> {
        self.models()
            .map(|models| models.into_iter().map(|model| model.id).collect())
    }

    pub const fn source_label(&self) -> &'static str {
        self.source.label()
    }

    pub(crate) fn models(&self) -> Result<Vec<ModelMetadata>, String> {
        source_models(self.source).map_err(|_| "model registry is unavailable".to_owned())
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn metadata_known(&self) -> bool {
        self.metadata_known
    }

    pub fn apply_model(&mut self, model: &str) -> Result<(), String> {
        if !self.models()?.iter().any(|candidate| candidate.id == model) {
            return Err(format!("model is unavailable for {}", self.source.label()));
        }

        self.model = model.to_owned();
        self.metadata_known = true;
        if self
            .reasoning_effort
            .is_some_and(|effort| !self.reasoning_effort_values().contains(&effort.as_str()))
        {
            self.reasoning_effort = None;
            self.request_config = RequestConfig::default();
        }
        Ok(())
    }

    pub fn apply_unverified_model(&mut self, model: &str) -> Result<(), String> {
        if !valid_model_id(model) {
            return Err("model identifier is invalid".to_owned());
        }

        self.model = model.to_owned();
        self.metadata_known = false;
        self.reasoning_effort = None;
        self.request_config = RequestConfig::default();
        Ok(())
    }

    pub fn reasoning_effort_values(&self) -> Vec<&'static str> {
        if is_gpt_5_6_model(&self.model) {
            return vec!["default", "none", "low", "medium", "high", "xhigh", "max"];
        }

        match (self.source, self.model.as_str()) {
            (
                TuiModelSource::ChatGptSubscription,
                "gpt-5.3-codex-spark" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.5",
            ) => {
                vec![
                    "default", "none", "minimal", "low", "medium", "high", "xhigh",
                ]
            }
            (TuiModelSource::OpenAiApi, "gpt-5.5") => {
                vec!["default", "none", "low", "medium", "high", "xhigh"]
            }
            (TuiModelSource::OpenAiApi, "o3" | "o4-mini") => {
                vec!["default", "none", "minimal", "low", "medium", "high"]
            }
            _ => vec!["default"],
        }
    }

    pub fn reasoning_effort_default(&self) -> Option<&'static str> {
        is_gpt_5_6_model(&self.model).then_some("medium")
    }

    pub fn reasoning_effort(&self) -> Option<&'static str> {
        self.reasoning_effort.map(ReasoningEffort::as_str)
    }

    pub const fn reasoning_effort_value(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    pub const fn request_config(&self) -> &RequestConfig {
        &self.request_config
    }

    pub fn apply_reasoning_effort(&mut self, effort: &str) -> Result<(), String> {
        if effort == "default" {
            self.reasoning_effort = None;
            self.request_config = RequestConfig::default();
            return Ok(());
        }
        if !self.reasoning_effort_values().contains(&effort) {
            return Err("reasoning effort is unsupported".to_owned());
        }

        let selected = RequestConfig::with_reasoning_effort(effort)
            .map_err(|_| "reasoning effort is unsupported".to_owned())?;
        let payload = if self.source == TuiModelSource::ChatGptSubscription && effort == "minimal" {
            "low"
        } else {
            effort
        };
        self.reasoning_effort = selected.reasoning_effort();
        self.request_config = RequestConfig::with_reasoning_effort(payload)
            .map_err(|_| "reasoning effort is unsupported".to_owned())?;
        Ok(())
    }
}

fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 64
        && model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn source_models(source: TuiModelSource) -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    let mut models = match source {
        TuiModelSource::OpenAiApi => {
            let mut models = bundled_openai_models()?;
            for model in &mut models {
                let (output, reasoning) = bundled_capabilities(&model.id);
                model.output = output;
                model.reasoning = reasoning;
            }
            models.push(pinned_model("gpt-5.5", "GPT-5.5", 272_000, 128_000, true));
            models
        }
        TuiModelSource::ChatGptSubscription => vec![
            pinned_model(
                "gpt-5.3-codex-spark",
                "GPT-5.3 Codex Spark",
                128_000,
                128_000,
                true,
            ),
            pinned_model("gpt-5.4", "GPT-5.4", 272_000, 128_000, true),
            pinned_model("gpt-5.4-mini", "GPT-5.4 mini", 272_000, 128_000, true),
            pinned_model("gpt-5.5", "GPT-5.5", 272_000, 128_000, true),
        ],
    };
    models.retain(|model| !is_gpt_5_6_model(&model.id));
    models.extend(official_gpt_5_6_models());
    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(models)
}

fn is_gpt_5_6_model(model: &str) -> bool {
    GPT_5_6_MODELS.iter().any(|(id, _)| *id == model)
}

fn official_gpt_5_6_models() -> Vec<ModelMetadata> {
    GPT_5_6_MODELS
        .into_iter()
        .map(|(id, name)| pinned_model(id, name, 1_050_000, 128_000, true))
        .collect()
}

// Grounded in references/pi-mono at f58c1156; the bundled snapshot remains unchanged.
fn bundled_capabilities(model: &str) -> (Option<u64>, Option<bool>) {
    match model {
        "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano" => (Some(32_768), Some(false)),
        "gpt-4o" | "gpt-4o-mini" => (Some(16_384), Some(false)),
        "o3" | "o4-mini" => (Some(100_000), Some(true)),
        _ => (None, None),
    }
}

fn pinned_model(id: &str, name: &str, context: u64, output: u64, reasoning: bool) -> ModelMetadata {
    ModelMetadata {
        id: id.to_owned(),
        name: Some(name.to_owned()),
        context: Some(context),
        output: Some(output),
        reasoning: Some(reasoning),
        input_price: None,
        output_price: None,
    }
}

pub(crate) fn bundled_snapshot_checksum() -> String {
    format!("{:x}", Sha256::digest(SNAPSHOT))
}

pub(crate) fn parse_models(snapshot: &[u8]) -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    let snapshot =
        serde_json::from_slice::<Snapshot>(snapshot).map_err(|_| ModelRegistryError::Schema)?;
    if snapshot.source.trim().is_empty() || snapshot.revision.trim().is_empty() {
        return Err(ModelRegistryError::Schema);
    }

    let mut models = snapshot
        .models
        .into_iter()
        .filter(|model| model.supported.unwrap_or(true))
        .filter_map(|model| {
            let id = model.id?.trim().to_owned();
            if id.is_empty() {
                return None;
            }

            Some(ModelMetadata {
                id,
                name: model.name.filter(|name| !name.trim().is_empty()),
                context: model.context,
                output: None,
                reasoning: None,
                input_price: model.input_price,
                output_price: model.output_price,
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(models)
}

pub(crate) fn format_models(models: &[ModelMetadata]) -> String {
    if models.is_empty() {
        return "No supported models.\n".to_owned();
    }

    let mut output = "ID\tNAME\tCONTEXT\tPRICE\n".to_owned();
    for model in models {
        let name = model.name.as_deref().unwrap_or("-");
        let context = model
            .context
            .map(|context| context.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let input = format_price(model.input_price);
        let output_price = format_price(model.output_price);

        output.push_str(&format!(
            "{}\t{name}\t{context}\t{input}/{output_price}\n",
            model.id
        ));
    }

    output
}

fn format_price(price: Option<f64>) -> String {
    price
        .map(|price| format!("${price:.2}"))
        .unwrap_or_else(|| "-".to_owned())
}

#[derive(Deserialize)]
struct Snapshot {
    source: String,
    revision: String,
    models: Vec<SnapshotModel>,
}

#[derive(Deserialize)]
struct SnapshotModel {
    id: Option<String>,
    name: Option<String>,
    context: Option<u64>,
    input_price: Option<f64>,
    output_price: Option<f64>,
    supported: Option<bool>,
}
