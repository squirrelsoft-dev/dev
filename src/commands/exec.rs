use std::path::Path;

use crate::devcontainer::DevcontainerConfig;
use crate::runtime::{ContainerState, detect_runtime};
use crate::util::{find_devcontainer_config, workspace_label};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    user: Option<&str>,
    cmd: &[String],
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let (label_key, label_val) = workspace_label(workspace);
    let filter = format!("{label_key}={label_val}");
    let containers = runtime.list_containers(&filter).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| anyhow::anyhow!("No running container found for this workspace. Run `dev up` first."))?;

    // Use explicit --user flag, falling back to remoteUser from config
    let config_user = if user.is_some() {
        None
    } else {
        find_devcontainer_config(workspace)
            .ok()
            .and_then(|p| DevcontainerConfig::from_path(&p).ok())
            .and_then(|c| c.remote_user)
    };
    let effective_user = user.or(config_user.as_deref());

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
