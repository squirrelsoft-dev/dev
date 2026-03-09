use std::collections::HashMap;

use dialoguer::{Input, MultiSelect, Select};

use crate::collection::{FeatureMetadata, OptionDef};
use crate::tui::{term_dimensions, truncate_to_width};

/// Interactively prompt the user for each template option.
pub fn prompt_options(options: &[OptionDef]) -> anyhow::Result<HashMap<String, String>> {
    let mut result = HashMap::new();

    for opt in options {
        let prompt_text = if opt.description.is_empty() {
            opt.id.clone()
        } else {
            format!("{} ({})", opt.id, opt.description)
        };

        let value = if let Some(ref enum_values) = opt.enum_values {
            // Strict enum — select from the list
            let default_idx = enum_values
                .iter()
                .position(|v| v == &opt.default)
                .unwrap_or(0);

            let dims = term_dimensions();
            let selection = Select::new()
                .with_prompt(&prompt_text)
                .items(enum_values)
                .default(default_idx)
                .max_length(dims.max_length)
                .interact()?;

            enum_values[selection].clone()
        } else if let Some(ref proposals) = opt.proposals {
            // Proposals — select from the list or enter custom value
            let mut items: Vec<String> = proposals.clone();
            items.push("Other (enter custom value)".to_string());

            let default_idx = proposals
                .iter()
                .position(|v| v == &opt.default)
                .unwrap_or(0);

            let dims = term_dimensions();
            let selection = Select::new()
                .with_prompt(&prompt_text)
                .items(&items)
                .default(default_idx)
                .max_length(dims.max_length)
                .interact()?;

            if selection < proposals.len() {
                proposals[selection].clone()
            } else {
                Input::new()
                    .with_prompt(&prompt_text)
                    .default(opt.default.clone())
                    .interact_text()?
            }
        } else {
            // Free text input
            Input::new()
                .with_prompt(&prompt_text)
                .default(opt.default.clone())
                .interact_text()?
        };

        result.insert(opt.id.clone(), value);
    }

    Ok(result)
}

/// Present a multi-select list of features. Each feature is paired with its
/// collection's OCI ref. Returns the selected feature refs
/// (e.g. `ghcr.io/devcontainers/features/node`).
///
/// `preselected` contains feature refs already present in the template config.
/// These will be pre-checked in the multi-select list.
pub fn multi_select_features(
    features: &[(String, FeatureMetadata)],
    preselected: &[String],
) -> anyhow::Result<Vec<String>> {
    if features.is_empty() {
        return Ok(Vec::new());
    }

    let dims = term_dimensions();
    let display_items: Vec<String> = features
        .iter()
        .map(|(oci_ref, f)| {
            let raw = if f.description.is_empty() {
                format!("{oci_ref}/{}", f.id)
            } else {
                format!("{oci_ref}/{} - {}", f.id, f.description)
            };
            truncate_to_width(&raw, dims.max_width)
        })
        .collect();

    // Pre-check features that are already in the template
    let defaults: Vec<bool> = features
        .iter()
        .map(|(oci_ref, f)| {
            let full_ref = format!("{oci_ref}/{}", f.id);
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
        .max_length(dims.max_length)
        .interact_opt()?
        .unwrap_or_default();

    Ok(selections
        .into_iter()
        .map(|i| {
            let (oci_ref, f) = &features[i];
            format!("{oci_ref}/{}", f.id)
        })
        .collect())
}
