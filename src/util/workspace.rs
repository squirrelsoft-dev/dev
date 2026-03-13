use std::path::{Path, PathBuf};

use crate::error::DevError;
use super::paths::devcontainers_dir;

/// Where a devcontainer config was found.
#[derive(Debug, Clone)]
pub enum ConfigSource {
    /// A concrete `devcontainer.json` ready to use.
    Direct(PathBuf),
    /// A `recipe.json` that needs composition before use.
    Recipe(PathBuf),
}

/// Find the config source for a workspace (recipe or direct config).
///
/// Search order:
/// 1. `<workspace>/.devcontainer/devcontainer.json` → Direct
/// 2. `<workspace>/.devcontainer.json` → Direct
/// 3. `~/.dev/devcontainers/<folder>/.devcontainer/recipe.json` → Recipe
/// 4. `~/.dev/devcontainers/<folder>/.devcontainer/devcontainer.json` → Direct (legacy)
pub fn find_config_source(workspace: &Path) -> Result<ConfigSource, DevError> {
    let nested = workspace.join(".devcontainer/devcontainer.json");
    if nested.is_file() {
        return Ok(ConfigSource::Direct(nested));
    }

    let root_level = workspace.join(".devcontainer.json");
    if root_level.is_file() {
        return Ok(ConfigSource::Direct(root_level));
    }

    let folder_name = workspace_folder_name(workspace);
    let user_dir = devcontainers_dir()
        .join(&folder_name)
        .join(".devcontainer");

    // Check for recipe first (new flow)
    let recipe_path = user_dir.join("recipe.json");
    if recipe_path.is_file() {
        return Ok(ConfigSource::Recipe(recipe_path));
    }

    // Legacy: direct devcontainer.json
    let user_scoped = user_dir.join("devcontainer.json");
    if user_scoped.is_file() {
        return Ok(ConfigSource::Direct(user_scoped));
    }

    Err(DevError::NoConfig(workspace.display().to_string()))
}

/// Locate the devcontainer configuration file for a workspace.
///
/// Search order:
/// 1. `<workspace>/.devcontainer/devcontainer.json`
/// 2. `<workspace>/.devcontainer.json`
/// 3. `~/.dev/devcontainers/<workspace-folder>/.devcontainer/recipe.json` (returns composed path)
/// 4. `~/.dev/devcontainers/<workspace-folder>/.devcontainer/devcontainer.json`
pub fn find_devcontainer_config(workspace: &Path) -> Result<PathBuf, DevError> {
    match find_config_source(workspace)? {
        ConfigSource::Direct(path) => Ok(path),
        ConfigSource::Recipe(_) => {
            // For recipe sources, the composed devcontainer.json should be at the
            // same location. The caller is responsible for composing it first.
            let folder_name = workspace_folder_name(workspace);
            let composed = devcontainers_dir()
                .join(&folder_name)
                .join(".devcontainer/devcontainer.json");
            Ok(composed)
        }
    }
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
