use std::fs;
use std::path::Path;

pub fn run(workspace: &Path) -> anyhow::Result<()> {
    let devcontainer_dir = workspace.join(".devcontainer");

    if devcontainer_dir.exists() {
        anyhow::bail!(".devcontainer already exists in {}", workspace.display());
    }

    fs::create_dir_all(&devcontainer_dir)?;

    let dockerfile = devcontainer_dir.join("Dockerfile");
    fs::write(
        &dockerfile,
        "FROM mcr.microsoft.com/devcontainers/base:ubuntu\n",
    )?;

    let devcontainer_json = devcontainer_dir.join("devcontainer.json");
    let config = serde_json::json!({
        "name": "Dev Container",
        "build": {
            "dockerfile": "Dockerfile"
        }
    });
    fs::write(&devcontainer_json, serde_json::to_string_pretty(&config)?)?;

    println!(
        "Created .devcontainer/ in {}",
        workspace.display()
    );
    Ok(())
}
