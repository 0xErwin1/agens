use serde::Deserialize;
use sha2::{Digest, Sha256};

const SNAPSHOT: &[u8] = include_bytes!("../data/models.dev-openai.json");
const SNAPSHOT_CHECKSUM: &str = include_str!("../data/models.dev-openai.json.sha256");

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelMetadata {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) context: Option<u64>,
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
