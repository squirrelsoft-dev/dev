use std::path::Path;

use crate::devcontainer::DevcontainerConfig;
use crate::runtime::{ContainerState, detect_runtime, resolve_remote_user};
use crate::util::{find_devcontainer_config, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    user: Option<&str>,
    cmd: &[String],
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| anyhow::anyhow!("No running container found for this workspace. Run `dev up` first."))?;

    // Use explicit --user flag, falling back to remoteUser from config or image metadata
    let resolved_user = if user.is_some() {
        user.map(|u| u.to_string())
    } else {
        let config_user = find_devcontainer_config(workspace)
            .ok()
            .and_then(|p| DevcontainerConfig::from_path(&p).ok())
            .and_then(|c| c.remote_user);
        resolve_remote_user(
            runtime.as_ref(),
            &container.image,
            config_user.as_deref(),
        ).await?
    };
    let effective_user = resolved_user.as_deref();

    let result = runtime.exec(&container.id, cmd, effective_user).await?;

    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    if result.exit_code != 0 {
        std::process::exit(result.exit_code);
    }

    Ok(())
}
