use std::path::Path;

use crate::runtime::{ContainerState, detect_runtime};
use crate::util::workspace_label;

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    remove: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let (label_key, label_val) = workspace_label(workspace);
    let filter = format!("{label_key}={label_val}");
    let containers = runtime.list_containers(&filter).await?;

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

    Ok(())
}
