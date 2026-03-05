use std::path::{Path, PathBuf};

use crate::error::DevError;
use super::paths::devcontainers_dir;

/// Locate the devcontainer configuration file for a workspace.
///
/// Search order:
/// 1. `<workspace>/.devcontainer/devcontainer.json`
/// 2. `<workspace>/.devcontainer.json`
/// 3. `~/.dev/devcontainers/<workspace-folder>/.devcontainer/devcontainer.json`
pub fn find_devcontainer_config(workspace: &Path) -> Result<PathBuf, DevError> {
    let nested = workspace.join(".devcontainer/devcontainer.json");
    if nested.is_file() {
        return Ok(nested);
    }

    let root_level = workspace.join(".devcontainer.json");
    if root_level.is_file() {
        return Ok(root_level);
    }

    // Check user-scoped config
    let folder_name = workspace_folder_name(workspace);
    let user_scoped = devcontainers_dir()
        .join(&folder_name)
        .join(".devcontainer/devcontainer.json");
    if user_scoped.is_file() {
        return Ok(user_scoped);
    }

    Err(DevError::NoConfig(workspace.display().to_string()))
}

/// Get just the directory name of the workspace path.
pub fn workspace_folder_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_workspace_folder_name() {
        assert_eq!(workspace_folder_name(Path::new("/home/user/my-project")), "my-project");
        assert_eq!(workspace_folder_name(Path::new("/tmp")), "tmp");
    }

    #[test]
    fn test_find_config_missing() {
        let result = find_devcontainer_config(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }
}
