use std::path::{Path, PathBuf};

use super::paths::devcontainers_dir;
use crate::error::DevError;

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
/// 1. `<workspace>/.devcontainer/recipe.json` → Recipe
/// 2. `<workspace>/.devcontainer/devcontainer.json` → Direct
/// 3. `<workspace>/.devcontainer.json` → Direct
/// 4. `~/.dev/devcontainers/<folder>/.devcontainer/recipe.json` → Recipe
/// 5. `~/.dev/devcontainers/<folder>/.devcontainer/devcontainer.json` → Direct (legacy)
pub fn find_config_source(workspace: &Path) -> Result<ConfigSource, DevError> {
    let workspace_recipe = workspace.join(".devcontainer/recipe.json");
    let nested = workspace.join(".devcontainer/devcontainer.json");
    let root_level = workspace.join(".devcontainer.json");

    if workspace_recipe.is_file() {
        if nested.is_file() || root_level.is_file() {
            return Err(DevError::InvalidConfig(format!(
                "Found both {} and a devcontainer.json for this workspace; keep either recipe.json or devcontainer.json, not both",
                workspace_recipe.display()
            )));
        }
        return Ok(ConfigSource::Recipe(workspace_recipe));
    }

    if nested.is_file() {
        return Ok(ConfigSource::Direct(nested));
    }

    if root_level.is_file() {
        return Ok(ConfigSource::Direct(root_level));
    }

    let folder_name = workspace_folder_name(workspace);
    let user_dir = devcontainers_dir().join(&folder_name).join(".devcontainer");

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
/// 1. `<workspace>/.devcontainer/recipe.json` (returns virtual composed path)
/// 2. `<workspace>/.devcontainer/devcontainer.json`
/// 3. `<workspace>/.devcontainer.json`
/// 4. `~/.dev/devcontainers/<workspace-folder>/.devcontainer/recipe.json` (returns virtual composed path)
/// 5. `~/.dev/devcontainers/<workspace-folder>/.devcontainer/devcontainer.json`
pub fn find_devcontainer_config(workspace: &Path) -> Result<PathBuf, DevError> {
    match find_config_source(workspace)? {
        ConfigSource::Direct(path) => Ok(path),
        ConfigSource::Recipe(path) => Ok(path.with_file_name("devcontainer.json")),
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
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn test_workspace_folder_name() {
        assert_eq!(
            workspace_folder_name(Path::new("/home/user/my-project")),
            "my-project"
        );
        assert_eq!(workspace_folder_name(Path::new("/tmp")), "tmp");
    }

    #[test]
    fn test_find_config_missing() {
        let result = find_devcontainer_config(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn workspace_recipe_is_a_config_source() {
        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let recipe_path = devcontainer_dir.join("recipe.json");
        fs::write(
            &recipe_path,
            r#"{"globalTemplate":"rust","rootFolder":"/tmp/demo"}"#,
        )
        .unwrap();

        match find_config_source(workspace.path()).unwrap() {
            ConfigSource::Recipe(path) => assert_eq!(path, recipe_path),
            other => panic!("expected workspace recipe source, got {other:?}"),
        }
        assert_eq!(
            find_devcontainer_config(workspace.path()).unwrap(),
            devcontainer_dir.join("devcontainer.json")
        );
    }

    #[test]
    fn workspace_recipe_conflicts_with_workspace_devcontainer_json() {
        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(devcontainer_dir.join("recipe.json"), "{}").unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();

        let err = find_config_source(workspace.path()).unwrap_err();

        assert!(
            err.to_string().contains("both"),
            "conflict should be explicit: {err}"
        );
    }

    #[test]
    fn workspace_direct_config_remains_a_direct_source() {
        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let direct = devcontainer_dir.join("devcontainer.json");
        fs::write(&direct, "{}").unwrap();

        match find_config_source(workspace.path()).unwrap() {
            ConfigSource::Direct(path) => assert_eq!(path, direct),
            other => panic!("expected direct source, got {other:?}"),
        }
    }
}
