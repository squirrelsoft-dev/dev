use std::fs;
use std::path::Path;

use crate::util::paths::{devcontainers_dir, vscode_configs_dir};
use crate::util::workspace::{find_config_source, ConfigSource};
use crate::util::workspace_folder_name;

/// Re-link VS Code's remote-containers entry for a legacy user-scoped
/// `devcontainer.json`.
///
/// The extension opens the linked directory and reads a `devcontainer.json` from
/// it, so linking a recipe-only directory would only recreate a broken entry.
pub fn repair(workspace: &Path) -> anyhow::Result<()> {
    let folder_name = workspace_folder_name(workspace);
    let target = devcontainers_dir().join(&folder_name);
    let config_path = user_scoped_direct_config(workspace, &target)?;

    let vscode_dir = vscode_configs_dir();
    if !vscode_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
        anyhow::bail!(
            "VS Code remote-containers extension not found at {}",
            vscode_dir.parent().unwrap().display()
        );
    }
    fs::create_dir_all(&vscode_dir)?;

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
        println!("Linked {}", config_path.display());
    }

    #[cfg(not(unix))]
    {
        let _ = config_path;
        anyhow::bail!("Symlink creation is only supported on Unix systems");
    }

    Ok(())
}

/// Resolve the workspace to a user-scoped `devcontainer.json`, rejecting every
/// other shape with the reason it cannot be linked.
fn user_scoped_direct_config(
    workspace: &Path,
    target: &Path,
) -> anyhow::Result<std::path::PathBuf> {
    let config_path = match find_config_source(workspace)? {
        ConfigSource::Direct(path) => path,
        ConfigSource::Recipe(recipe_path) => anyhow::bail!(
            "{} is a recipe, so there is no devcontainer.json for VS Code to open and \
             linking it would leave a broken entry.\n\
             Run `dev up` and attach your editor to the running container instead.",
            recipe_path.display()
        ),
    };

    if !config_path.starts_with(target) {
        anyhow::bail!(
            "{} lives in the workspace, which VS Code already opens on its own — \
             only user-scoped configs under {} need a link.",
            config_path.display(),
            devcontainers_dir().display()
        );
    }

    Ok(config_path)
}
