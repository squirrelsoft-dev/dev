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
                    workspace,
                    &config,
                    &config_path,
                    runtime.runtime_name(),
                    remove,
                )
                .await;
            }
        }
    }

    // Non-compose: label-based container stop/remove.
    run_with_runtime(workspace, &*runtime, remove).await
}

/// Internal: run the non-compose teardown path against a specific runtime.
/// Exposed for tests.
pub async fn run_with_runtime(
    workspace: &Path,
    runtime: &dyn crate::runtime::ContainerRuntime,
    remove: bool,
) -> anyhow::Result<()> {
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    if containers.is_empty() {
        println!("No containers found for this workspace.");
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();

    for container in &containers {
        let stopped = if container.state == ContainerState::Running {
            eprintln!("Stopping container '{}'...", container.name);
            stop_container_safe(runtime, &container.id).await
        } else {
            Ok(())
        };

        if remove {
            eprintln!("Removing container '{}'...", container.name);
            if let Err(e) = runtime.remove_container(&container.id).await {
                if let Err(stop_err) = &stopped {
                    failures.push(format!(
                        "stop_container {} failed: {}",
                        container.name, stop_err,
                    ));
                }
                failures.push(format!("remove_container {} failed: {}", container.name, e,));
            } else {
                println!("Container '{}' removed.", container.name);
            }
        } else {
            match stopped {
                Ok(()) => println!("Container '{}' stopped.", container.name),
                Err(e) => {
                    failures.push(format!("stop_container {} failed: {}", container.name, e,));
                }
            }
        }
    }

    if !failures.is_empty() {
        let err = anyhow::anyhow!("{}", failures.join("; "));
        Err(err)
    } else if let Err(e) = crate::caddy::unregister_site(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
        Ok(())
    } else {
        Ok(())
    }
}

/// Stop a container, but trust observed state over the call's return value.
///
/// If `stop_container` returns `Ok`, the call succeeded (no re-check needed).
/// If it returns `Err`, we re-check via `inspect_container`: if the container
/// is no longer `Running`, we treat it as actually stopped and swallow the
/// error. Only treat a failure as genuine if the container is still running
/// after the re-check.
async fn stop_container_safe(
    runtime: &dyn crate::runtime::ContainerRuntime,
    id: &str,
) -> anyhow::Result<()> {
    if runtime.stop_container(id).await.is_ok() {
        return Ok(());
    }

    // Re-check state: if it stopped while reporting failure, honour that.
    match runtime.inspect_container(id).await {
        Ok(info) => {
            if info.state != ContainerState::Running {
                return Ok(());
            }
        }
        Err(e) => {
            eprintln!("Warning: inspect_container failed after stop_container: {e}");
        }
    }
    anyhow::bail!(
        "stop_container failed and container {} still appears running",
        id
    );
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
            runtime_name,
            &compose_files,
            devcontainer_dir,
            &project_name,
        )
        .await?;
        println!("Compose services removed.");
    } else {
        eprintln!("Stopping compose services...");
        crate::runtime::compose::compose_stop(
            runtime_name,
            &compose_files,
            devcontainer_dir,
            &project_name,
        )
        .await?;
        println!("Compose services stopped.");
    }

    if let Err(e) = crate::caddy::unregister_site(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
    }

    Ok(())
}
