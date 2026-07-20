use std::path::Path;

use crate::runtime::detect_runtime;
use crate::util::{find_config_source, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    // Check for devcontainer config (informational only)
    if find_config_source(workspace).is_err() && !json {
        eprintln!(
            "No devcontainer configuration found in {}",
            workspace.display()
        );
    }

    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

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
        println!("{:<30} {:<12} IMAGE", "NAME", "STATE");
        for c in &containers {
            println!(
                "{:<30} {:<12} {}",
                c.name,
                format!("{:?}", c.state),
                c.image
            );
        }
    }

    Ok(())
}
