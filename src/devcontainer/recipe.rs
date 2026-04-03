use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A lightweight recipe that references a global template by name and stores
/// project-specific overrides. The full `devcontainer.json` is composed at
/// build/up time by merging layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipe {
    /// Name of the global template (e.g. "rust"), located at `~/.dev/global/<name>/`
    pub global_template: String,
    /// Additional feature OCI references to inject
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Template option substitutions (`${templateOption:key}` → value)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub options: HashMap<String, String>,
    /// Absolute path to the workspace root
    pub root_folder: String,
    /// User-applied configuration overrides (highest-priority layer during composition).
    /// Stores devcontainer.json property deltas set via `dev config set/add/remove`.
    #[serde(
        default = "default_empty_object",
        skip_serializing_if = "is_empty_object"
    )]
    pub customizations: Value,
}

fn default_empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

pub fn is_empty_object(v: &Value) -> bool {
    v.as_object().map(|m| m.is_empty()).unwrap_or(true)
}

impl Recipe {
    /// Read a recipe from a `recipe.json` file.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        let recipe: Recipe = serde_json::from_str(&raw)?;
        Ok(recipe)
    }

    /// Write this recipe to a `recipe.json` file.
    pub fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let formatted = serde_json::to_string_pretty(self)?;
        fs::write(path, formatted)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_recipe_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("recipe.json");

        let recipe = Recipe {
            global_template: "rust".to_string(),
            features: vec!["ghcr.io/features/zsh:1".to_string()],
            options: HashMap::from([("imageVariant".to_string(), "bookworm".to_string())]),
            root_folder: "/home/user/project".to_string(),
            customizations: default_empty_object(),
        };

        recipe.write_to(&path).unwrap();
        let loaded = Recipe::from_path(&path).unwrap();

        assert_eq!(loaded.global_template, "rust");
        assert_eq!(loaded.features.len(), 1);
        assert_eq!(loaded.options["imageVariant"], "bookworm");
        assert_eq!(loaded.root_folder, "/home/user/project");
    }

    #[test]
    fn test_recipe_minimal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("recipe.json");

        let recipe = Recipe {
            global_template: "python".to_string(),
            features: Vec::new(),
            options: HashMap::new(),
            root_folder: "/tmp/proj".to_string(),
            customizations: default_empty_object(),
        };

        recipe.write_to(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        // Empty vecs/maps/objects should be omitted
        assert!(!raw.contains("features"));
        assert!(!raw.contains("options"));
        assert!(!raw.contains("customizations"));

        let loaded = Recipe::from_path(&path).unwrap();
        assert_eq!(loaded.global_template, "python");
        assert!(loaded.features.is_empty());
        assert!(loaded.options.is_empty());
        assert!(is_empty_object(&loaded.customizations));
    }

    #[test]
    fn test_recipe_customizations_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("recipe.json");

        let customizations = serde_json::json!({
            "remoteUser": "vscode",
            "forwardPorts": [9090],
            "remoteEnv": {"MY_VAR": "hello"}
        });

        let recipe = Recipe {
            global_template: "rust".to_string(),
            features: Vec::new(),
            options: HashMap::new(),
            root_folder: "/home/user/project".to_string(),
            customizations,
        };

        recipe.write_to(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("customizations"));

        let loaded = Recipe::from_path(&path).unwrap();
        assert_eq!(loaded.customizations["remoteUser"], "vscode");
        assert_eq!(loaded.customizations["forwardPorts"][0], 9090);
        assert_eq!(loaded.customizations["remoteEnv"]["MY_VAR"], "hello");
    }

    #[test]
    fn test_recipe_without_customizations_field_deserializes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("recipe.json");
        // Simulate a legacy recipe.json without the customizations field
        fs::write(
            &path,
            r#"{"globalTemplate": "rust", "rootFolder": "/tmp/proj"}"#,
        )
        .unwrap();

        let loaded = Recipe::from_path(&path).unwrap();
        assert_eq!(loaded.global_template, "rust");
        assert!(is_empty_object(&loaded.customizations));
    }
}
