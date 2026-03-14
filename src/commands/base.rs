use std::fs;

use crate::cli::ConfigAction;
use crate::collection::{fetch_all_features, fetch_collection_index};
use crate::tui::prompts;
use crate::util::paths::base_config_dir;

/// Returns the path to the base devcontainer.json config file.
fn base_config_path() -> std::path::PathBuf {
    base_config_dir().join("devcontainer.json")
}

/// Read feature refs from the existing base config, if any.
fn read_existing_features() -> Vec<String> {
    let raw = match fs::read_to_string(base_config_path()) {
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

/// Interactively create or update the base config with feature selection.
pub async fn new(verbose: u8) -> anyhow::Result<()> {
    eprintln!("Fetching feature catalog...");
    let collections = fetch_collection_index(false).await?;

    let existing_features = read_existing_features();

    let features = fetch_all_features(&collections, false).await;
    let selected_features = if features.is_empty() {
        eprintln!("No features available.");
        Vec::new()
    } else {
        prompts::multi_select_features(&features, &existing_features)?
    };

    // Build the config, preserving existing fields if the file already exists
    let config_path = base_config_path();
    let mut json = if config_path.is_file() {
        let raw = fs::read_to_string(&config_path)?;
        serde_json::from_str::<serde_json::Value>(&raw)?
    } else {
        fs::create_dir_all(base_config_dir())?;
        serde_json::json!({
            "features": {},
            "mounts": [],
            "remoteEnv": {}
        })
    };

    // Replace features with the selected set, preserving existing option values
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("base config is not a JSON object"))?;

    let old_features = obj
        .get("features")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default();

    let mut new_features = serde_json::Map::new();
    for feature_ref in &selected_features {
        let opts = old_features
            .get(feature_ref)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        new_features.insert(feature_ref.clone(), opts);
    }
    obj.insert(
        "features".to_string(),
        serde_json::Value::Object(new_features),
    );

    fs::write(&config_path, serde_json::to_string_pretty(&json)?)?;

    if verbose > 0 {
        eprintln!(
            "Selected {} feature(s)",
            selected_features.len()
        );
    }
    println!("Base config written to {}", config_path.display());
    Ok(())
}

/// Open the base config in $EDITOR, creating a scaffold if it doesn't exist.
pub fn edit() -> anyhow::Result<()> {
    let config_path = base_config_path();

    if !config_path.is_file() {
        // Create scaffold
        fs::create_dir_all(base_config_dir())?;
        let scaffold = serde_json::json!({
            "features": {},
            "mounts": [],
            "remoteEnv": {}
        });
        fs::write(&config_path, serde_json::to_string_pretty(&scaffold)?)?;
        eprintln!("Created base config at {}", config_path.display());
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

/// View or modify the base devcontainer configuration.
/// Delegates to the shared config command logic.
pub async fn config(action: Option<ConfigAction>, verbose: u8) -> anyhow::Result<()> {
    let config_path = base_config_path();

    if !config_path.is_file() {
        // Create scaffold so config commands have something to work with
        fs::create_dir_all(base_config_dir())?;
        let scaffold = serde_json::json!({
            "features": {},
            "mounts": [],
            "remoteEnv": {}
        });
        fs::write(&config_path, serde_json::to_string_pretty(&scaffold)?)?;
        eprintln!("Created base config at {}", config_path.display());
    }

    super::config::run(&config_path, action, verbose).await
}
