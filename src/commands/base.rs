use std::fs;

use crate::cli::ConfigAction;
use crate::util::paths::base_config_dir;

/// Returns the path to the base devcontainer.json config file.
fn base_config_path() -> std::path::PathBuf {
    base_config_dir().join("devcontainer.json")
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
