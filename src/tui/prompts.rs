use std::collections::HashMap;

use dialoguer::{Input, MultiSelect, Select};

use crate::collection::{FeatureMetadata, OptionDef};

/// Interactively prompt the user for each template option.
pub fn prompt_options(options: &[OptionDef]) -> anyhow::Result<HashMap<String, String>> {
    let mut result = HashMap::new();

    for opt in options {
        let value = if let Some(ref enum_values) = opt.enum_values {
            // Present a selection for enum options
            let default_idx = enum_values
                .iter()
                .position(|v| v == &opt.default)
                .unwrap_or(0);

            let prompt_text = if opt.description.is_empty() {
                opt.id.clone()
            } else {
                format!("{} ({})", opt.id, opt.description)
            };

            let selection = Select::new()
                .with_prompt(&prompt_text)
                .items(enum_values)
                .default(default_idx)
                .interact()?;

            enum_values[selection].clone()
        } else {
            let prompt_text = if opt.description.is_empty() {
                opt.id.clone()
            } else {
                format!("{} ({})", opt.id, opt.description)
            };

            Input::new()
                .with_prompt(&prompt_text)
                .default(opt.default.clone())
                .interact_text()?
        };

        result.insert(opt.id.clone(), value);
    }

    Ok(result)
}

/// Present a multi-select list of features. Returns the selected feature IDs
/// (full OCI references like `ghcr.io/devcontainers/features/node`).
///
/// `preselected` contains feature refs already present in the template config.
/// These will be pre-checked in the multi-select list.
pub fn multi_select_features(
    features: &[FeatureMetadata],
    collection_oci_ref: &str,
    preselected: &[String],
) -> anyhow::Result<Vec<String>> {
    if features.is_empty() {
        return Ok(Vec::new());
    }

    let display_items: Vec<String> = features
        .iter()
        .map(|f| {
            if f.description.is_empty() {
                format!("{collection_oci_ref}/{}", f.id)
            } else {
                format!("{collection_oci_ref}/{} - {}", f.id, f.description)
            }
        })
        .collect();

    // Pre-check features that are already in the template
    let defaults: Vec<bool> = features
        .iter()
        .map(|f| {
            let full_ref = format!("{collection_oci_ref}/{}", f.id);
            preselected.iter().any(|p| {
                // Match against full ref or just the feature id suffix
                p == &full_ref || p.ends_with(&format!("/{}", f.id))
            })
        })
        .collect();

    let selections = MultiSelect::new()
        .with_prompt("Add features (space to toggle, enter to confirm)")
        .items(&display_items)
        .defaults(&defaults)
        .interact_opt()?
        .unwrap_or_default();

    Ok(selections
        .into_iter()
        .map(|i| format!("{collection_oci_ref}/{}", features[i].id))
        .collect())
}
