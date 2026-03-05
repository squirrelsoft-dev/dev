use std::path::Path;

use crate::runtime::detect_runtime;
use crate::util::{find_devcontainer_config, workspace_label};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    // Check for devcontainer config (informational only)
    if find_devcontainer_config(workspace).is_err() {
        if !json {
            eprintln!("No devcontainer configuration found in {}", workspace.display());
        }
    }

    let runtime = detect_runtime(runtime_override).await?;
    let (label_key, label_val) = workspace_label(workspace);
    let filter = format!("{label_key}={label_val}");
    let containers = runtime.list_containers(&filter).await?;

    if json {
        let items: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "state": format!("{:?}", c.state),
                    "image": c.image,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if containers.is_empty() {
        println!("No containers running for this workspace.");
        println!("Use `dev up` to start a container for this workspace.");
    } else {
        println!("{:<30} {:<12} {}", "NAME", "STATE", "IMAGE");
        for c in &containers {
            println!("{:<30} {:<12} {}", c.name, format!("{:?}", c.state), c.image);
        }
    }

    Ok(())
}
