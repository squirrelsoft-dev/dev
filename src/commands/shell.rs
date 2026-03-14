use std::path::Path;

use crate::devcontainer::DevcontainerConfig;
use crate::runtime::{ContainerState, detect_runtime, resolve_remote_user};
use crate::util::{find_devcontainer_config, workspace_folder_name, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    shell: Option<&str>,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| anyhow::anyhow!("No running container found for this workspace. Run `dev up` first."))?;

    // Resolve remoteUser from config or image metadata
    let config_user = find_devcontainer_config(workspace)
        .ok()
        .and_then(|p| DevcontainerConfig::from_path(&p).ok())
        .and_then(|c| c.remote_user);
    let user = resolve_remote_user(
        runtime.as_ref(),
        &container.image,
        config_user.as_deref(),
    ).await?;

    let shell_cmd = if let Some(s) = shell {
        s.to_string()
    } else {
        // Probe for available shells
        let candidates = ["/bin/zsh", "/bin/bash", "/bin/sh"];
        let mut found = None;
        for candidate in &candidates {
            let probe = vec!["test".to_string(), "-x".to_string(), candidate.to_string()];
            let result = runtime.exec(&container.id, &probe, user.as_deref()).await?;
            if result.exit_code == 0 {
                found = Some(candidate.to_string());
                break;
            }
        }
        found.unwrap_or_else(|| "/bin/sh".to_string())
    };

    // Start the shell in the workspace folder
    let folder_name = workspace_folder_name(workspace);
    let workdir = format!("/workspaces/{folder_name}");
    let cmd = vec![
        shell_cmd.clone(),
        "-c".to_string(),
        format!("cd {workdir} 2>/dev/null; exec {shell_cmd} -l"),
    ];
    runtime
        .exec_interactive(&container.id, &cmd, user.as_deref())
        .await?;

    Ok(())
}
