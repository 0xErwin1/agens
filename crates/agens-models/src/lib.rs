use agens_core::{ReasoningEffort, RequestConfig};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SNAPSHOT: &[u8] = include_bytes!("../data/models.dev-openai.json");
const SNAPSHOT_CHECKSUM: &str = include_str!("../data/models.dev-openai.json.sha256");
const MOONSHOT_SNAPSHOT: &[u8] = include_bytes!("../data/models.moonshotai.json");
const MOONSHOT_SNAPSHOT_CHECKSUM: &str = include_str!("../data/models.moonshotai.json.sha256");
const GPT_5_6_MODELS: [(&str, &str); 4] = [
    ("gpt-5.6", "GPT-5.6 (Sol alias)"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct ModelMetadata {
    pub id: String,
    pub name: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<bool>,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    /// Non-default reasoning effort levels the provider reports as valid for
    /// this model, excluding the implicit `"default"` level every model accepts.
    /// Empty when the source has no per-model effort capability data.
    pub reasoning_efforts: Vec<&'static str>,
    /// Declared input modalities (for example `text`, `image`, `pdf`, `video`).
    /// Empty means modalities are unknown — fail closed when attachments are present.
    pub input_modalities: Vec<String>,
    /// Optional explicit input MIME allowlist. When non-empty, only listed MIMEs
    /// are accepted even if a broader modality would otherwise match.
    pub input_mimes: Vec<String>,
}

impl ModelMetadata {
    /// Returns whether this model accepts the given input MIME type.
    ///
    /// - `Some(true)` — catalog declares support via modality or explicit MIME
    /// - `Some(false)` — catalog is known and does not support this MIME
    /// - `None` — modalities unknown (missing from catalog)
    pub fn accepts_input_mime(&self, mime: &str) -> Option<bool> {
        if self.input_modalities.is_empty() && self.input_mimes.is_empty() {
            return None;
        }

        let mime = mime.trim();
        if mime.is_empty() {
            return Some(false);
        }

        let normalized = mime.to_ascii_lowercase();

        if !self.input_mimes.is_empty() {
            let allowed = self
                .input_mimes
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&normalized));
            return Some(allowed);
        }

        let Some(modality) = modality_for_mime(&normalized) else {
            return Some(false);
        };

        Some(
            self.input_modalities
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(modality)),
        )
    }
}

/// Looks up a model across bundled catalogs and returns input MIME acceptance.
///
/// Returns `None` when the model is unknown or its modalities were not pinned.
pub fn accepts_input_mime(model_id: &str, mime: &str) -> Option<bool> {
    metadata_for(model_id).and_then(|model| model.accepts_input_mime(mime))
}

fn metadata_for(model_id: &str) -> Option<ModelMetadata> {
    for source in [
        ModelSource::OpenAiApi,
        ModelSource::ChatGptSubscription,
        ModelSource::MoonshotApi,
    ] {
        if let Ok(models) = source_models(source)
            && let Some(model) = models.into_iter().find(|model| model.id == model_id)
        {
            return Some(model);
        }
    }

    None
}

/// Maps a MIME type onto a catalog modality label.
///
/// Unknown MIME families return `None` so a known catalog rejects them.
fn modality_for_mime(mime: &str) -> Option<&'static str> {
    if mime == "application/pdf" {
        return Some("pdf");
    }
    if let Some((top_level, _)) = mime.split_once('/') {
        return match top_level {
            "image" => Some("image"),
            "video" => Some("video"),
            "audio" => Some("audio"),
            "text" => Some("text"),
            _ => None,
        };
    }
    None
}

#[derive(Debug)]
pub enum ModelRegistryError {
    Checksum,
    Schema,
}

/// The bundled snapshot's source and revision are recorded in its JSON metadata.
pub fn bundled_openai_models() -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    if bundled_snapshot_checksum() != SNAPSHOT_CHECKSUM.trim() {
        return Err(ModelRegistryError::Checksum);
    }

    parse_models(SNAPSHOT)
}

/// Loads the bundled Moonshot model catalog snapshot.
///
/// Regenerate in three steps. The key itself is never recorded anywhere in the
/// snapshot, the checksum, or this comment.
///
/// 1. Fetch the live catalog:
///    `curl -s https://api.moonshot.ai/v1/models -H "Authorization: Bearer $MOONSHOT_API_KEY" > /tmp/moonshot-models.json`
/// 2. Distill the API's `{"object":"list","data":[...]}` envelope into this
///    file's `{source, revision, models:[{id, context_length, reasoning_efforts}]}`
///    shape:
///    `jq '{source: "https://api.moonshot.ai/v1/models", revision: (now | strftime("%Y-%m-%d")), models: [.data[] | {id, context_length, reasoning_efforts: (if .reasoning_efforts.support then {support: true, valid_efforts: .reasoning_efforts.valid_efforts, default_effort: .reasoning_efforts.default_effort} else null end)}]}' /tmp/moonshot-models.json > crates/agens-models/data/models.moonshotai.json`
/// 3. Recompute the checksum:
///    `sha256sum crates/agens-models/data/models.moonshotai.json | cut -d' ' -f1 | tr -d '\n' > crates/agens-models/data/models.moonshotai.json.sha256`
pub fn bundled_moonshot_models() -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    if moonshot_snapshot_checksum() != MOONSHOT_SNAPSHOT_CHECKSUM.trim() {
        return Err(ModelRegistryError::Checksum);
    }

    parse_moonshot_models(MOONSHOT_SNAPSHOT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    OpenAiApi,
    ChatGptSubscription,
    MoonshotApi,
}

impl ModelSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenAiApi => "OpenAI API",
            Self::ChatGptSubscription => "ChatGPT subscription",
            Self::MoonshotApi => "Moonshot AI",
        }
    }

    /// The identifier a remembered selection is stored under.
    ///
    /// Deliberately separate from [`Self::label`]: labels are display text and may be reworded,
    /// which would silently orphan every preference already written under the old wording.
    pub const fn storage_key(self) -> &'static str {
        match self {
            Self::OpenAiApi => "openai-api",
            Self::ChatGptSubscription => "chatgpt-subscription",
            Self::MoonshotApi => "moonshot-api",
        }
    }
}

/// Validates and retains the bounded selections exposed by the terminal UI adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    model: String,
    source: ModelSource,
    metadata_known: bool,
    reasoning_effort: Option<ReasoningEffort>,
    request_config: RequestConfig,
}

impl ModelSelection {
    pub fn new(model: impl Into<String>) -> Self {
        Self::for_source(model, ModelSource::OpenAiApi)
    }

    pub fn for_source(model: impl Into<String>, source: ModelSource) -> Self {
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

    pub const fn source(&self) -> ModelSource {
        self.source
    }

    pub const fn source_label(&self) -> &'static str {
        self.source.label()
    }

    pub fn models(&self) -> Result<Vec<ModelMetadata>, String> {
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
                ModelSource::ChatGptSubscription,
                "gpt-5.3-codex-spark" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.5",
            ) => {
                vec![
                    "default", "none", "minimal", "low", "medium", "high", "xhigh",
                ]
            }
            (ModelSource::OpenAiApi, "gpt-5.5") => {
                vec!["default", "none", "low", "medium", "high", "xhigh"]
            }
            (ModelSource::OpenAiApi, "o3" | "o4-mini") => {
                vec!["default", "none", "minimal", "low", "medium", "high"]
            }
            (ModelSource::MoonshotApi, model_id) => self
                .models()
                .ok()
                .and_then(|models| models.into_iter().find(|model| model.id == model_id))
                .map(|model| {
                    let mut efforts = vec!["default"];
                    efforts.extend(model.reasoning_efforts);
                    efforts
                })
                .unwrap_or_else(|| vec!["default"]),
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
        let payload = if self.source == ModelSource::ChatGptSubscription && effort == "minimal" {
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

/// Looks up a known model's context window from the registry.
///
/// Returns `None` when the model is unknown or has no recorded window.
/// Never invents a default size.
pub fn context_window_for(model_id: &str) -> Option<u64> {
    for source in [
        ModelSource::OpenAiApi,
        ModelSource::ChatGptSubscription,
        ModelSource::MoonshotApi,
    ] {
        if let Ok(models) = source_models(source)
            && let Some(model) = models.iter().find(|model| model.id == model_id)
        {
            return model.context;
        }
    }

    None
}

fn source_models(source: ModelSource) -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    let mut models = match source {
        ModelSource::OpenAiApi => {
            let mut models = bundled_openai_models()?;
            for model in &mut models {
                let (output, reasoning) = bundled_capabilities(&model.id);
                model.output = output;
                model.reasoning = reasoning;
            }
            models.push(pinned_model("gpt-5.5", "GPT-5.5", 272_000, 128_000, true));
            models
        }
        ModelSource::ChatGptSubscription => vec![
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
        ModelSource::MoonshotApi => {
            let mut models = bundled_moonshot_models()?;
            models.sort_by(|left, right| left.id.cmp(&right.id));
            return Ok(models);
        }
    };
    // The GPT-5.6 alias fixup only ever applies to OpenAI-dialect sources; the
    // early return above keeps a Moonshot list from inheriting four GPT models.
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
    pinned_model_with_modalities(id, name, context, output, reasoning, &["text", "image"])
}

fn pinned_model_with_modalities(
    id: &str,
    name: &str,
    context: u64,
    output: u64,
    reasoning: bool,
    modalities: &[&str],
) -> ModelMetadata {
    ModelMetadata {
        id: id.to_owned(),
        name: Some(name.to_owned()),
        context: Some(context),
        output: Some(output),
        reasoning: Some(reasoning),
        input_price: None,
        output_price: None,
        reasoning_efforts: Vec::new(),
        input_modalities: modalities
            .iter()
            .map(|modality| (*modality).to_owned())
            .collect(),
        input_mimes: Vec::new(),
    }
}

pub fn bundled_snapshot_checksum() -> String {
    format!("{:x}", Sha256::digest(SNAPSHOT))
}

pub fn moonshot_snapshot_checksum() -> String {
    format!("{:x}", Sha256::digest(MOONSHOT_SNAPSHOT))
}

fn parse_moonshot_models(snapshot: &[u8]) -> Result<Vec<ModelMetadata>, ModelRegistryError> {
    let snapshot = serde_json::from_slice::<MoonshotSnapshot>(snapshot)
        .map_err(|_| ModelRegistryError::Schema)?;
    if snapshot.source.trim().is_empty() || snapshot.revision.trim().is_empty() {
        return Err(ModelRegistryError::Schema);
    }

    Ok(snapshot
        .models
        .into_iter()
        .map(|model| ModelMetadata {
            id: model.id,
            name: None,
            context: Some(model.context_length),
            output: None,
            reasoning: Some(true),
            input_price: None,
            output_price: None,
            reasoning_efforts: model
                .reasoning_efforts
                .filter(|efforts| efforts.support)
                .map(|efforts| {
                    efforts
                        .valid_efforts
                        .iter()
                        .filter_map(|effort| static_effort_label(effort))
                        .collect()
                })
                .unwrap_or_default(),
            input_modalities: model.input_modalities,
            input_mimes: model.input_mimes,
        })
        .collect())
}

/// Maps a reasoning effort level from provider data onto the fixed vocabulary
/// this crate already uses elsewhere (`reasoning_effort_values`,
/// `RequestConfig::with_reasoning_effort`). Unrecognized levels are dropped
/// rather than surfaced, since a caller can only select from this vocabulary.
fn static_effort_label(effort: &str) -> Option<&'static str> {
    match effort {
        "default" => Some("default"),
        "none" => Some("none"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

pub fn parse_models(snapshot: &[u8]) -> Result<Vec<ModelMetadata>, ModelRegistryError> {
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
                reasoning_efforts: Vec::new(),
                input_modalities: model.input_modalities,
                input_mimes: model.input_mimes,
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(models)
}

pub fn format_models(models: &[ModelMetadata]) -> String {
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
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    input_mimes: Vec<String>,
}

#[derive(Deserialize)]
struct MoonshotSnapshot {
    source: String,
    revision: String,
    models: Vec<MoonshotSnapshotModel>,
}

#[derive(Deserialize)]
struct MoonshotSnapshotModel {
    id: String,
    context_length: u64,
    #[serde(default)]
    reasoning_efforts: Option<MoonshotReasoningEfforts>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    input_mimes: Vec<String>,
}

/// `default_effort` is part of the wire shape but is not read here: Agens'
/// own `"default"` reasoning-effort level is provider-agnostic and always
/// unwinds to `RequestConfig::default()`, so the provider's preferred effort
/// has nothing to attach to yet.
#[derive(Deserialize)]
struct MoonshotReasoningEfforts {
    support: bool,
    valid_efforts: Vec<String>,
}

/// The model a run falls back to when nothing selected one. Takes the provider
/// identifier rather than a resolved configuration: the choice depends on which
/// provider is in play and on nothing else.
///
/// Returns `None` for an unrecognized or absent provider. There is deliberately
/// no catch-all: guessing a model for an unknown provider would route the run,
/// and the billing that follows it, to whichever provider the guess happened to
/// name.
pub fn default_model(provider_type: Option<&str>) -> Option<&'static str> {
    match provider_type? {
        "openai-api" => Some("gpt-4.1"),
        "openai-chatgpt" => Some("gpt-5.5"),
        "moonshotai" => Some("kimi-k3"),
        _ => None,
    }
}

/// The provider identifiers [`default_model`] resolves, for error messages that
/// tell the user what they may write instead of what they wrote.
pub const KNOWN_PROVIDER_TYPES: [&str; 3] = ["openai-api", "openai-chatgpt", "moonshotai"];

/// Explains a [`default_model`] miss in the terms the user can act on: what they
/// configured, and what the accepted values are.
pub fn unknown_provider_message(provider_type: Option<&str>) -> String {
    let valid = KNOWN_PROVIDER_TYPES.join(", ");

    match provider_type {
        Some(configured) if !configured.is_empty() => {
            format!("provider.type \"{configured}\" is not supported; valid values are {valid}")
        }
        _ => format!("provider.type is not configured; valid values are {valid}"),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn parses_tolerant_snapshot_filters_and_sorts_models() {
        let snapshot = br#"{
                "source": "https://models.dev",
                "revision": "test",
                "models": [
                    {"id":"z-model","name":"Z","context":4,"input_price":1.5,"output_price":2.5,"supported":true,"future":true},
                    {"id":"a-model","supported":true},
                    {"id":"unsupported","supported":false},
                    {"name":"missing-id","supported":true}
                ]
            }"#;

        let models = crate::parse_models(snapshot).expect("snapshot parses");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "a-model");
        assert_eq!(models[0].name, None);
        assert_eq!(models[0].context, None);
        assert_eq!(models[0].input_price, None);
        assert_eq!(models[0].output_price, None);
        assert_eq!(models[1].id, "z-model");
    }

    #[test]
    fn validates_bundled_snapshot_checksum_and_schema() {
        let models = crate::bundled_openai_models().expect("bundled snapshot is valid");

        assert_eq!(
            crate::bundled_snapshot_checksum(),
            "30146e401e82d0ec0230f40319650691762cd54401b966598af27bd1868bc56a"
        );
        assert_eq!(
            models.first().map(|model| model.id.as_str()),
            Some("gpt-4.1")
        );
        assert_eq!(
            models.last().map(|model| model.id.as_str()),
            Some("o4-mini")
        );
    }

    #[test]
    fn rejects_snapshot_schema_without_a_model_collection() {
        let result = crate::parse_models(br#"{"source":"https://models.dev","revision":"test"}"#);

        assert!(result.is_err());
    }

    #[test]
    fn formats_four_columns_and_an_explicit_empty_result() {
        let output = crate::format_models(&[
            crate::ModelMetadata {
                id: "missing".to_owned(),
                name: None,
                context: None,
                output: None,
                reasoning: None,
                input_price: None,
                output_price: Some(0.6),
                reasoning_efforts: Vec::new(),
                input_modalities: Vec::new(),
                input_mimes: Vec::new(),
            },
            crate::ModelMetadata {
                id: "known".to_owned(),
                name: Some("Known".to_owned()),
                context: Some(128000),
                output: None,
                reasoning: None,
                input_price: Some(2.5),
                output_price: Some(10.0),
                reasoning_efforts: Vec::new(),
                input_modalities: Vec::new(),
                input_mimes: Vec::new(),
            },
        ]);

        assert_eq!(
            output,
            "ID\tNAME\tCONTEXT\tPRICE\nmissing\t-\t-\t-/$0.60\nknown\tKnown\t128000\t$2.50/$10.00\n"
        );
        assert_eq!(crate::format_models(&[]), "No supported models.\n");
    }

    #[test]
    fn context_window_for_returns_registry_value_or_none() {
        assert_eq!(crate::context_window_for("gpt-4.1"), Some(1_047_576));
        assert_eq!(crate::context_window_for("gpt-5.5"), Some(272_000));
        assert_eq!(crate::context_window_for("not-a-real-model-xyz"), None);
    }

    #[test]
    fn moonshot_catalog_matches_pinned_list() {
        let selector =
            crate::ModelSelection::for_source("kimi-k3", crate::ModelSource::MoonshotApi);
        let models = selector.models().expect("moonshot catalog is available");

        let mut ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "kimi-k2.6",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k3",
            ]
        );

        let by_id = |id: &str| models.iter().find(|model| model.id == id).unwrap();
        assert_eq!(by_id("kimi-k3").context, Some(1_048_576));
        assert_eq!(by_id("kimi-k2.6").context, Some(262_144));
        assert_eq!(by_id("kimi-k2.7-code").context, Some(262_144));
        assert_eq!(by_id("kimi-k2.7-code-highspeed").context, Some(262_144));
    }

    #[test]
    fn moonshot_list_excludes_gpt_models() {
        let selector =
            crate::ModelSelection::for_source("kimi-k3", crate::ModelSource::MoonshotApi);
        let models = selector.models().expect("moonshot catalog is available");

        assert!(
            models.iter().all(|model| !model.id.starts_with("gpt-")),
            "moonshot catalog must never carry a GPT model: {models:?}"
        );
    }

    #[test]
    fn moonshot_reasoning_effort_values() {
        let k3 = crate::ModelSelection::for_source("kimi-k3", crate::ModelSource::MoonshotApi);
        assert_eq!(
            k3.reasoning_effort_values(),
            vec!["default", "low", "high", "max"]
        );

        for model in ["kimi-k2.6", "kimi-k2.7-code", "kimi-k2.7-code-highspeed"] {
            let selector =
                crate::ModelSelection::for_source(model, crate::ModelSource::MoonshotApi);
            assert_eq!(selector.reasoning_effort_values(), vec!["default"]);
        }
    }

    #[test]
    fn moonshot_models_never_report_a_fabricated_output_limit() {
        let selector =
            crate::ModelSelection::for_source("kimi-k3", crate::ModelSource::MoonshotApi);
        let models = selector.models().expect("moonshot catalog is available");

        assert!(
            models.iter().all(|model| model.output.is_none()),
            "moonshot catalog must never invent an output token limit: {models:?}"
        );
    }

    #[test]
    fn moonshot_reasoning_effort_values_come_from_the_snapshot_not_a_hardcoded_id() {
        let k3 = crate::ModelSelection::for_source("kimi-k3", crate::ModelSource::MoonshotApi);
        assert_eq!(
            k3.reasoning_effort_values(),
            vec!["default", "low", "high", "max"]
        );

        let unknown_model =
            crate::ModelSelection::for_source("kimi-k4", crate::ModelSource::MoonshotApi);
        assert_eq!(unknown_model.reasoning_effort_values(), vec!["default"]);
    }

    #[test]
    fn moonshot_context_window() {
        assert_eq!(crate::context_window_for("kimi-k3"), Some(1_048_576));
        assert_eq!(crate::context_window_for("kimi-k2.6"), Some(262_144));
        assert_eq!(crate::context_window_for("kimi-k2.7-code"), Some(262_144));
        assert_eq!(
            crate::context_window_for("kimi-k2.7-code-highspeed"),
            Some(262_144)
        );
    }

    #[test]
    fn default_model_resolves_every_known_provider() {
        assert_eq!(crate::default_model(Some("openai-api")), Some("gpt-4.1"));
        assert_eq!(
            crate::default_model(Some("openai-chatgpt")),
            Some("gpt-5.5")
        );
        assert_eq!(crate::default_model(Some("moonshotai")), Some("kimi-k3"));
    }

    #[test]
    fn default_model_refuses_to_guess_for_an_unknown_provider() {
        assert_eq!(crate::default_model(Some("moonshot")), None);
        assert_eq!(crate::default_model(Some("")), None);
        assert_eq!(crate::default_model(None), None);
    }

    #[test]
    fn accepts_input_mime_is_unknown_when_modalities_are_absent() {
        let model = crate::ModelMetadata {
            id: "unknown-modalities".to_owned(),
            name: None,
            context: None,
            output: None,
            reasoning: None,
            input_price: None,
            output_price: None,
            reasoning_efforts: Vec::new(),
            input_modalities: Vec::new(),
            input_mimes: Vec::new(),
        };

        assert_eq!(model.accepts_input_mime("image/png"), None);
        assert_eq!(
            crate::accepts_input_mime("not-a-real-model-xyz", "image/png"),
            None
        );
    }

    #[test]
    fn accepts_input_mime_returns_true_for_supported_modality_and_false_for_unsupported() {
        let model = crate::ModelMetadata {
            id: "vision".to_owned(),
            name: None,
            context: None,
            output: None,
            reasoning: None,
            input_price: None,
            output_price: None,
            reasoning_efforts: Vec::new(),
            input_modalities: vec!["text".into(), "image".into()],
            input_mimes: Vec::new(),
        };

        assert_eq!(model.accepts_input_mime("image/png"), Some(true));
        assert_eq!(model.accepts_input_mime("IMAGE/JPEG"), Some(true));
        assert_eq!(model.accepts_input_mime("application/pdf"), Some(false));
        assert_eq!(model.accepts_input_mime("video/mp4"), Some(false));
    }

    #[test]
    fn accepts_input_mime_honors_explicit_mime_allowlist() {
        let model = crate::ModelMetadata {
            id: "narrow".to_owned(),
            name: None,
            context: None,
            output: None,
            reasoning: None,
            input_price: None,
            output_price: None,
            reasoning_efforts: Vec::new(),
            input_modalities: vec!["image".into()],
            input_mimes: vec!["image/png".into()],
        };

        assert_eq!(model.accepts_input_mime("image/png"), Some(true));
        assert_eq!(model.accepts_input_mime("image/jpeg"), Some(false));
    }

    #[test]
    fn parse_models_reads_input_modalities_and_mimes() {
        let snapshot = br#"{
                "source": "https://models.dev",
                "revision": "test",
                "models": [
                    {
                        "id": "with-modalities",
                        "supported": true,
                        "input_modalities": ["text", "image", "pdf"],
                        "input_mimes": ["image/png", "application/pdf"]
                    },
                    {
                        "id": "without-modalities",
                        "supported": true
                    }
                ]
            }"#;

        let models = crate::parse_models(snapshot).expect("snapshot parses");
        let with = models
            .iter()
            .find(|model| model.id == "with-modalities")
            .expect("with-modalities present");
        let without = models
            .iter()
            .find(|model| model.id == "without-modalities")
            .expect("without-modalities present");

        assert_eq!(
            with.input_modalities,
            vec!["text".to_owned(), "image".to_owned(), "pdf".to_owned()]
        );
        assert_eq!(
            with.input_mimes,
            vec!["image/png".to_owned(), "application/pdf".to_owned()]
        );
        assert!(without.input_modalities.is_empty());
        assert!(without.input_mimes.is_empty());
        assert_eq!(with.accepts_input_mime("image/png"), Some(true));
        assert_eq!(without.accepts_input_mime("image/png"), None);
    }

    #[test]
    fn bundled_catalog_pins_modalities_for_known_models() {
        assert_eq!(
            crate::accepts_input_mime("gpt-4.1", "image/png"),
            Some(true)
        );
        assert_eq!(
            crate::accepts_input_mime("gpt-4.1", "application/pdf"),
            Some(true)
        );
        assert_eq!(
            crate::accepts_input_mime("gpt-4.1-nano", "application/pdf"),
            Some(false)
        );
        assert_eq!(
            crate::accepts_input_mime("kimi-k3", "image/jpeg"),
            Some(true)
        );
        assert_eq!(
            crate::accepts_input_mime("kimi-k3", "video/mp4"),
            Some(true)
        );
        assert_eq!(
            crate::accepts_input_mime("kimi-k3", "application/pdf"),
            Some(false)
        );
        assert_eq!(
            crate::accepts_input_mime("gpt-5.5", "image/png"),
            Some(true)
        );
    }
}
