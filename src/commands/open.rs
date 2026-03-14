use std::path::{Path, PathBuf};

use crate::runtime::{ContainerState, detect_runtime};
use crate::util::{find_config_source, workspace_folder_name, workspace_labels, ConfigSource};
use crate::util::paths::devcontainers_dir;

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    insiders: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let _container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No running container found for this workspace. Run `dev up` first."
            )
        })?;

    let binary = if insiders { "code-insiders" } else { "code" };
    verify_binary_exists(binary).await?;

    let abs_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let folder_name = workspace_folder_name(workspace);

    // Build the VS Code URI. When the config lives outside the workspace
    // (user-scoped), encode a JSON object with a configFile property so
    // VS Code passes --config to the devcontainer CLI.
    let config_file = resolve_external_config(workspace);
    let hex = if let Some(config_path) = config_file {
        let json = serde_json::json!({
            "hostPath": abs_path.to_string_lossy(),
            "configFile": {
                "$mid": 1,
                "path": config_path.to_string_lossy(),
                "scheme": "file"
            }
        });
        hex::encode(json.to_string().as_bytes())
    } else {
        hex::encode(abs_path.to_string_lossy().as_bytes())
    };

    let uri = format!(
        "vscode-remote://dev-container+{hex}/workspaces/{folder_name}"
    );

    eprintln!("Opening VS Code attached to container...");
    let status = tokio::process::Command::new(binary)
        .arg("--folder-uri")
        .arg(&uri)
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("{binary} exited with status {status}");
    }

    Ok(())
}

/// If the devcontainer config lives outside the workspace (user-scoped),
/// return its absolute path so it can be embedded in the VS Code URI.
/// Returns `None` when the config is already in the workspace.
fn resolve_external_config(workspace: &Path) -> Option<PathBuf> {
    match find_config_source(workspace) {
        Ok(ConfigSource::Direct(path)) => {
            // Config is in the workspace — no need for configFile.
            if path.starts_with(workspace) {
                return None;
            }
            // Direct config outside the workspace (legacy user-scoped).
            Some(path)
        }
        Ok(ConfigSource::Recipe(_)) => {
            // Recipe-based: the composed devcontainer.json is in ~/.dev/devcontainers/.
            let folder_name = workspace_folder_name(workspace);
            let composed = devcontainers_dir()
                .join(&folder_name)
                .join(".devcontainer/devcontainer.json");
            if composed.is_file() {
                Some(composed)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

async fn verify_binary_exists(binary: &str) -> anyhow::Result<()> {
    let result = tokio::process::Command::new("which")
        .arg(binary)
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => Ok(()),
        _ => anyhow::bail!(
            "'{binary}' not found in PATH. Install VS Code and ensure the '{binary}' \
             command is available (run \"Shell Command: Install 'code' command in PATH\" \
             from the VS Code command palette)."
        ),
    }
}
