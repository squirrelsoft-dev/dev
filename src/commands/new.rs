use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::collection::{
    fetch_all_features, fetch_collection_index, fetch_templates, Collection, TemplateMetadata,
    template_collections, template_tier, TemplateTier,
};
use crate::devcontainer::apply_template;
use crate::oci::download_artifact;
use crate::tui::{picker, prompts};
use crate::util::paths::{devcontainers_dir, global_dir, vscode_configs_dir};
use crate::util::workspace_folder_name;

pub async fn run(
    workspace: &Path,
    template: Option<&str>,
    options: &[String],
    verbose: u8,
) -> anyhow::Result<()> {
    eprintln!("Fetching template catalog...");
    let collections = fetch_collection_index(false).await?;

    // Check for existing global templates
    let global_names = list_global_template_names();
    let has_globals = !global_names.is_empty();

    // If --template was passed, use the old direct flow (skip source picker)
    let (oci_ref, selected) = if let Some(id) = template {
        let all_templates = fetch_all_templates(&collections, verbose).await?;
        all_templates
            .into_iter()
            .find(|(_, t)| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("Template '{id}' not found"))?
    } else {
        // Interactive source picker
        let source = picker::pick_source(has_globals)?;

        match source {
            picker::TemplateSource::ExistingGlobal => {
                let name = picker::pick_global_template(&global_names)?;
                return apply_global_template(workspace, &name, options, verbose).await;
            }
            _ => {
                let templates = fetch_templates_for_source(source, &collections, verbose).await?;
                if templates.is_empty() {
                    anyhow::bail!("No templates available in the selected category.");
                }
                let (oci_ref, t) = picker::pick_template(&templates)?;
                (oci_ref, t.clone())
            }
        }
    };

    // Feature multi-select
    let selected_features = select_features(&collections, &[], verbose).await?;

    // Scope picker
    let scope = picker::pick_scope()?;

    // Download the template artifact
    let version = if selected.version.is_empty() {
        "latest"
    } else {
        &selected.version
    };
    let artifact_ref = format!("{}/{}", oci_ref, selected.id);
    eprintln!("Downloading template '{}'...", selected.id);
    let artifact_path = download_artifact(&artifact_ref, version).await?;

    // Parse options from CLI args or prompt interactively
    let mut opts = parse_option_args(options);
    if !selected.options.is_empty() {
        let missing: Vec<_> = selected
            .options
            .iter()
            .filter(|o| !opts.contains_key(&o.id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let prompted = prompts::prompt_options(&missing)?;
            opts.extend(prompted);
        }
    }

    // Apply template to the chosen scope
    let dest = match scope {
        picker::Scope::Workspace => {
            apply_template(&artifact_path, &opts, workspace)?;
            workspace.to_path_buf()
        }
        picker::Scope::User => {
            let folder_name = workspace_folder_name(workspace);
            let user_dest = devcontainers_dir().join(&folder_name);
            fs::create_dir_all(&user_dest)?;
            apply_template(&artifact_path, &opts, &user_dest)?;

            // Create .devcontainer-internal.json with rootFolder
            let internal = user_dest.join(".devcontainer-internal.json");
            let internal_json = serde_json::json!({
                "rootFolder": workspace.to_string_lossy()
            });
            fs::write(&internal, serde_json::to_string_pretty(&internal_json)?)?;

            // Create VS Code symlink if the configs dir exists
            create_vscode_symlink(&folder_name, &user_dest);

            user_dest
        }
    };

    // Inject selected features
    if !selected_features.is_empty() {
        inject_features_into_config(&dest, &selected_features)?;
    }

    // Merge base config if it exists
    if crate::devcontainer::merge::merge_base_config(&dest)? {
        eprintln!("Merged base config from ~/.dev/base/devcontainer.json");
    }

    println!("Template '{}' applied to {}", selected.id, dest.display());
    Ok(())
}

/// Apply an existing global template to the workspace.
async fn apply_global_template(
    workspace: &Path,
    global_name: &str,
    options: &[String],
    verbose: u8,
) -> anyhow::Result<()> {
    let global_path = global_dir().join(global_name);
    if !global_path.is_dir() {
        anyhow::bail!("Global template '{global_name}' not found");
    }

    // Read existing features from the global template's devcontainer.json
    let existing_features = read_existing_features(&global_path);

    // Feature multi-select with existing features pre-checked
    eprintln!("Fetching feature catalog...");
    let collections = fetch_collection_index(false).await?;
    let selected_features =
        select_features(&collections, &existing_features, verbose).await?;

    let scope = picker::pick_scope()?;
    let opts = parse_option_args(options);

    let dest = match scope {
        picker::Scope::Workspace => {
            apply_template(&global_path, &opts, workspace)?;
            workspace.to_path_buf()
        }
        picker::Scope::User => {
            let folder_name = workspace_folder_name(workspace);
            let user_dest = devcontainers_dir().join(&folder_name);
            fs::create_dir_all(&user_dest)?;
            apply_template(&global_path, &opts, &user_dest)?;

            let internal = user_dest.join(".devcontainer-internal.json");
            let internal_json = serde_json::json!({
                "rootFolder": workspace.to_string_lossy()
            });
            fs::write(&internal, serde_json::to_string_pretty(&internal_json)?)?;

            create_vscode_symlink(&folder_name, &user_dest);

            user_dest
        }
    };

    // Replace features with the user's full selection (overwrite existing features
    // since the multi-select already included them as pre-checked defaults)
    if !selected_features.is_empty() || !existing_features.is_empty() {
        replace_features_in_config(&dest, &selected_features)?;
    }

    // Merge base config if it exists
    if crate::devcontainer::merge::merge_base_config(&dest)? {
        eprintln!("Merged base config from ~/.dev/base/devcontainer.json");
    }

    println!(
        "Global template '{global_name}' applied to {}",
        dest.display()
    );
    Ok(())
}

/// Fetch templates only from the collections matching the chosen source category.
async fn fetch_templates_for_source(
    source: picker::TemplateSource,
    collections: &[Collection],
    verbose: u8,
) -> anyhow::Result<Vec<(String, TemplateMetadata)>> {
    let all_template_cols = template_collections(collections);
    let target_tier = match source {
        picker::TemplateSource::Official => TemplateTier::Official,
        picker::TemplateSource::Microsoft => TemplateTier::Microsoft,
        picker::TemplateSource::Community => TemplateTier::Community,
        picker::TemplateSource::ExistingGlobal => {
            unreachable!("handled before this function is called");
        }
    };
    let filtered: Vec<&&Collection> = all_template_cols
        .iter()
        .filter(|c| template_tier(c) == target_tier)
        .collect();

    let mut templates = Vec::new();
    for c in &filtered {
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
    Ok(templates)
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
/// `preselected` contains feature refs already in the template (pre-checked in the list).
async fn select_features(
    collections: &[Collection],
    preselected: &[String],
    _verbose: u8,
) -> anyhow::Result<Vec<String>> {
    let features = fetch_all_features(collections, false).await;
    if features.is_empty() {
        return Ok(Vec::new());
    }
    prompts::multi_select_features(&features, preselected)
}

/// Get the list of global template names from ~/.dev/global/.
fn list_global_template_names() -> Vec<String> {
    let dir = global_dir();
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    names.sort();
    names
}

/// Create a symlink from VS Code's remote-containers configs dir to the user-scoped config.
fn create_vscode_symlink(folder_name: &str, target: &Path) {
    let vscode_dir = vscode_configs_dir();
    if !vscode_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
        // VS Code remote-containers extension not installed
        return;
    }

    let _ = fs::create_dir_all(&vscode_dir);
    let link_path = vscode_dir.join(folder_name);
    if link_path.exists() {
        return;
    }

    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(target, &link_path);
    }
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

/// Read feature refs from an existing devcontainer.json's "features" map.
fn read_existing_features(template_dir: &Path) -> Vec<String> {
    let config_path = template_dir.join(".devcontainer/devcontainer.json");
    let raw = match fs::read_to_string(&config_path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    json.get("features")
        .and_then(|f| f.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// Replace the "features" field in devcontainer.json with exactly the given set.
fn replace_features_in_config(dest: &Path, features: &[String]) -> anyhow::Result<()> {
    let config_path = dest.join(".devcontainer/devcontainer.json");
    if !config_path.is_file() {
        return Ok(());
    }

    let raw = fs::read_to_string(&config_path)?;
    let mut json: serde_json::Value = serde_json::from_str(&raw)?;

    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    let mut new_features = serde_json::Map::new();
    for feature_ref in features {
        // Preserve existing options if the feature was already present
        let existing_opts = obj
            .get("features")
            .and_then(|f| f.as_object())
            .and_then(|f| f.get(feature_ref))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        new_features.insert(feature_ref.clone(), existing_opts);
    }
    obj.insert(
        "features".to_string(),
        serde_json::Value::Object(new_features),
    );

    let formatted = serde_json::to_string_pretty(&json)?;
    fs::write(&config_path, formatted)?;
    Ok(())
}

fn parse_option_args(args: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for arg in args {
        if let Some((key, val)) = arg.split_once('=') {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}
