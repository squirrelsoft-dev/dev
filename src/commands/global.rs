use std::collections::HashMap;
use std::fs;
use std::path::Path;

use dialoguer::{Confirm, Input};

use crate::cli::ConfigAction;
use crate::collection::{
    fetch_all_features, fetch_collection_index, fetch_templates, Collection, TemplateMetadata,
    template_collections, template_tier, TemplateTier,
};
use crate::devcontainer::apply_template;
use crate::oci::download_artifact;
use crate::tui::{picker, prompts};
use crate::util::paths::global_dir;

/// Create a new global template at `~/.dev/global/<name>/`.
pub async fn new(
    template: Option<&str>,
    name: Option<&str>,
    verbose: u8,
) -> anyhow::Result<()> {
    // Fetch template catalog
    eprintln!("Fetching template catalog...");
    let collections = fetch_collection_index(false).await?;

    // Determine source and select template
    let (oci_ref, selected) =
        select_template_from_registry(template, &collections, verbose).await?;

    // Feature multi-select
    let selected_features = select_features(&collections, verbose).await?;

    // Get the name for the global template
    let template_name = if let Some(n) = name {
        n.to_string()
    } else {
        Input::new()
            .with_prompt("Name for this global template")
            .default(selected.id.clone())
            .interact_text()?
    };

    // Download and apply the template
    let version = if selected.version.is_empty() {
        "latest"
    } else {
        &selected.version
    };
    let artifact_ref = format!("{}/{}", oci_ref, selected.id);
    eprintln!("Downloading template '{}'...", selected.id);
    let artifact_path = download_artifact(&artifact_ref, version).await?;

    // Prompt for template options
    let mut opts = HashMap::new();
    if !selected.options.is_empty() {
        let prompted = prompts::prompt_options(&selected.options)?;
        opts.extend(prompted);
    }

    // Apply to global dir
    let dest = global_dir().join(&template_name);
    fs::create_dir_all(&dest)?;
    apply_template(&artifact_path, &opts, &dest)?;

    // Inject selected features into devcontainer.json
    if !selected_features.is_empty() {
        inject_features_into_config(&dest, &selected_features)?;
    }

    println!(
        "Global template '{}' created at {}",
        template_name,
        dest.display()
    );
    Ok(())
}

/// List all global templates.
pub fn list() -> anyhow::Result<()> {
    let dir = global_dir();
    if !dir.is_dir() {
        println!("No global templates found.");
        return Ok(());
    }

    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            let config_path = entry
                .path()
                .join(".devcontainer/devcontainer.json");
            let detail = if config_path.is_file() {
                peek_config_image(&config_path).unwrap_or_default()
            } else {
                String::new()
            };
            if detail.is_empty() {
                entries.push(name);
            } else {
                entries.push(format!("{name}  ({detail})"));
            }
        }
    }

    if entries.is_empty() {
        println!("No global templates found.");
    } else {
        for entry in &entries {
            println!("  {entry}");
        }
    }
    Ok(())
}

/// Open a global template's devcontainer.json in $EDITOR.
pub fn edit(name: &str) -> anyhow::Result<()> {
    let config_path = global_dir()
        .join(name)
        .join(".devcontainer/devcontainer.json");

    if !config_path.is_file() {
        anyhow::bail!("Global template '{name}' not found");
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&config_path)
        .status()?;

    if !status.success() {
        anyhow::bail!("{editor} exited with status {status}");
    }
    Ok(())
}

/// Remove a global template with confirmation.
pub fn remove(name: &str) -> anyhow::Result<()> {
    let template_dir = global_dir().join(name);
    if !template_dir.is_dir() {
        anyhow::bail!("Global template '{name}' not found");
    }

    let confirmed = Confirm::new()
        .with_prompt(format!("Remove global template '{name}'?"))
        .default(false)
        .interact()?;

    if confirmed {
        fs::remove_dir_all(&template_dir)?;
        println!("Removed global template '{name}'.");
    } else {
        println!("Cancelled.");
    }
    Ok(())
}

/// View or modify a global template's devcontainer configuration.
pub async fn config(
    name: &str,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    let config_path = global_dir()
        .join(name)
        .join(".devcontainer/devcontainer.json");

    if !config_path.is_file() {
        anyhow::bail!("Global template '{name}' not found");
    }

    super::config::run(&config_path, action, verbose).await
}

// --- Internal helpers ---

/// Select a template from the registry using the source picker flow.
async fn select_template_from_registry(
    template_id: Option<&str>,
    collections: &[Collection],
    verbose: u8,
) -> anyhow::Result<(String, TemplateMetadata)> {
    if let Some(id) = template_id {
        // Fetch all templates to find the one matching the ID
        let all_templates = fetch_all_templates(collections, verbose).await?;
        let (oci_ref, meta) = all_templates
            .into_iter()
            .find(|(_, t)| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("Template '{id}' not found"))?;

        return Ok((oci_ref, meta));
    }

    // Interactive: pick source category first
    let source = picker::pick_source(false)?;

    let all_template_cols = template_collections(collections);
    let target_tier = match source {
        picker::TemplateSource::Official => TemplateTier::Official,
        picker::TemplateSource::Microsoft => TemplateTier::Microsoft,
        picker::TemplateSource::Community => TemplateTier::Community,
        picker::TemplateSource::ExistingGlobal => {
            unreachable!("ExistingGlobal not available in global new");
        }
    };
    let filtered_collections: Vec<&&Collection> = all_template_cols
        .iter()
        .filter(|c| template_tier(c) == target_tier)
        .collect();

    let mut templates: Vec<(String, TemplateMetadata)> = Vec::new();
    for c in &filtered_collections {
        match fetch_templates(c, false).await {
            Ok(ts) => {
                for t in ts {
                    templates.push((c.oci_ref.clone(), t));
                }
            }
            Err(e) => {
                if verbose > 0 {
                    eprintln!("Warning: failed to fetch from '{}': {e}", c.name);
                }
            }
        }
    }

    if templates.is_empty() {
        anyhow::bail!("No templates available in the selected category.");
    }

    let (oci_ref, selected) = picker::pick_template(&templates)?;

    Ok((oci_ref, selected.clone()))
}

/// Fetch all templates from template collections.
async fn fetch_all_templates(
    collections: &[Collection],
    verbose: u8,
) -> anyhow::Result<Vec<(String, TemplateMetadata)>> {
    let template_cols = template_collections(collections);
    let fetches: Vec<_> = template_cols.iter().map(|c| fetch_templates(c, false)).collect();
    let results = futures_util::future::join_all(fetches).await;

    let mut all = Vec::new();
    for (collection, result) in template_cols.iter().zip(results) {
        match result {
            Ok(templates) => {
                for t in templates {
                    all.push((collection.oci_ref.clone(), t));
                }
            }
            Err(e) => {
                if verbose > 0 {
                    eprintln!("Warning: failed to fetch from '{}': {e}", collection.name);
                }
            }
        }
    }
    Ok(all)
}

/// Fetch features from all feature collections and present multi-select.
async fn select_features(
    collections: &[Collection],
    _verbose: u8,
) -> anyhow::Result<Vec<String>> {
    let features = fetch_all_features(collections, false).await;
    if features.is_empty() {
        return Ok(Vec::new());
    }
    prompts::multi_select_features(&features, &[])
}

/// Inject feature references into devcontainer.json's "features" field.
fn inject_features_into_config(dest: &Path, features: &[String]) -> anyhow::Result<()> {
    let config_path = dest.join(".devcontainer/devcontainer.json");
    if !config_path.is_file() {
        return Ok(());
    }

    let raw = fs::read_to_string(&config_path)?;
    let mut json: serde_json::Value = serde_json::from_str(&raw)?;

    let features_obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?
        .entry("features")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    if let Some(obj) = features_obj.as_object_mut() {
        for feature_ref in features {
            obj.insert(feature_ref.clone(), serde_json::json!({}));
        }
    }

    let formatted = serde_json::to_string_pretty(&json)?;
    fs::write(&config_path, formatted)?;
    Ok(())
}

/// Peek at the image or name from a devcontainer.json for display.
fn peek_config_image(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("image")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}
