use std::path::Path;

use crate::devcontainer::DevcontainerConfig;
use crate::runtime::{ContainerState, detect_runtime};
use crate::util::{container_name, find_devcontainer_config, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    remove: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;

    // Try compose-aware teardown first.
    if let Ok(config_path) = find_devcontainer_config(workspace) {
        if let Ok(config) = DevcontainerConfig::from_path(&config_path) {
            if config.is_compose() {
                return run_compose_down(
                    workspace, &config, &config_path,
                    runtime.runtime_name(), remove,
                ).await;
            }
        }
    }

    // Non-compose: label-based container stop/remove.
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    if containers.is_empty() {
        println!("No containers found for this workspace.");
        return Ok(());
    }

    for container in &containers {
        if container.state == ContainerState::Running {
            eprintln!("Stopping container '{}'...", container.name);
            runtime.stop_container(&container.id).await?;
        }

        if remove {
            eprintln!("Removing container '{}'...", container.name);
            runtime.remove_container(&container.id).await?;
            println!("Container '{}' removed.", container.name);
        } else {
            println!("Container '{}' stopped.", container.name);
        }
    }

    if let Err(e) = crate::caddy::unregister_site(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
    }

    Ok(())
}

/// Tear down a Docker Compose-based workspace.
async fn run_compose_down(
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
    runtime_name: &str,
    remove: bool,
) -> anyhow::Result<()> {
    let compose_data = config.docker_compose_file.as_ref().unwrap();
    let compose_files = compose_data.files();
    let devcontainer_dir = config_path.parent().unwrap();
    let project_name = container_name(workspace);

    if remove {
        eprintln!("Removing compose services...");
        crate::runtime::compose::compose_down(
            runtime_name, &compose_files, devcontainer_dir, &project_name,
        ).await?;
        println!("Compose services removed.");
    } else {
        eprintln!("Stopping compose services...");
        crate::runtime::compose::compose_stop(
            runtime_name, &compose_files, devcontainer_dir, &project_name,
        ).await?;
        println!("Compose services stopped.");
    }

    if let Err(e) = crate::caddy::unregister_site(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
    }

    Ok(())
}
