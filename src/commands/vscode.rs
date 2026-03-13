use std::fs;
use std::path::Path;

use crate::util::paths::{devcontainers_dir, vscode_configs_dir};

pub fn repair(workspace: &Path) -> anyhow::Result<()> {
    let vscode_dir = vscode_configs_dir();

    if !vscode_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
        anyhow::bail!(
            "VS Code remote-containers extension not found at {}",
            vscode_dir.parent().unwrap().display()
        );
    }

    fs::create_dir_all(&vscode_dir)?;

    let folder_name = crate::util::workspace_folder_name(workspace);
    let target = devcontainers_dir().join(&folder_name);

    if !target.is_dir() {
        anyhow::bail!(
            "No user-scoped config found for '{}' at {}",
            folder_name,
            target.display()
        );
    }

    let link_path = vscode_dir.join(&folder_name);

    // Remove existing entry (symlink, file, or directory)
    if link_path.symlink_metadata().is_ok() {
        if link_path.is_dir() && !link_path.is_symlink() {
            fs::remove_dir_all(&link_path)?;
        } else {
            fs::remove_file(&link_path)?;
        }
        println!("Removed existing {}", link_path.display());
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link_path)?;
        println!(
            "Created symlink {} -> {}",
            link_path.display(),
            target.display()
        );
    }

    #[cfg(not(unix))]
    {
        anyhow::bail!("Symlink creation is only supported on Unix systems");
    }

    Ok(())
}
