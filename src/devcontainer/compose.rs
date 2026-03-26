use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::devcontainer::jsonc::parse_jsonc;
use crate::devcontainer::merge::merge_layers;
use crate::devcontainer::recipe::Recipe;
use crate::util::paths::{base_config_dir, global_dir, runtime_config_dir};

/// Read a JSON file, stripping comments and trailing commas. Returns `None` if the file doesn't exist.
fn read_json_file(path: &Path) -> anyhow::Result<Option<Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    let value: Value = parse_jsonc(&raw)?;
    Ok(Some(value))
}

/// Apply `${templateOption:key}` substitutions throughout a JSON value.
fn substitute_template_options(value: &mut Value, options: &HashMap<String, String>) {
    match value {
        Value::String(s) => {
            for (key, val) in options {
                let placeholder = format!("${{templateOption:{key}}}");
                if s.contains(&placeholder) {
                    *s = s.replace(&placeholder, val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                substitute_template_options(item, options);
            }
        }
        Value::Object(map) => {
            for (_, v) in map {
                substitute_template_options(v, options);
            }
        }
        _ => {}
    }
}

/// Compose a final devcontainer config from a recipe and runtime name by merging layers:
/// 1. Global template (lowest priority)
/// 2. Base config (`~/.dev/base/devcontainer.json`)
/// 3. Runtime config (`~/.dev/<runtime>/devcontainer.json`) (highest priority for scalars)
///
/// Then inject recipe features into the composed result.
pub fn compose_config(recipe: &Recipe, runtime_name: &str) -> anyhow::Result<Value> {
    // Layer 1: Global template
    let global_config_path = global_dir()
        .join(&recipe.global_template)
        .join(".devcontainer/devcontainer.json");
    let mut global = read_json_file(&global_config_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Global template '{}' not found at {}",
            recipe.global_template,
            global_config_path.display()
        )
    })?;

    // Apply template option substitutions to the global template
    if !recipe.options.is_empty() {
        substitute_template_options(&mut global, &recipe.options);
    }

    // Layer 2: Base config
    let base_config_path = base_config_dir().join("devcontainer.json");
    let base = read_json_file(&base_config_path)?;

    // Layer 3: Runtime config
    let runtime_config_path = runtime_config_dir(runtime_name).join("devcontainer.json");
    let runtime = read_json_file(&runtime_config_path)?;

    // Merge layers in priority order
    let mut layers = vec![global];
    if let Some(b) = base {
        layers.push(b);
    }
    if let Some(r) = runtime {
        layers.push(r);
    }

    let mut composed = merge_layers(&layers);

    // Inject recipe features
    if !recipe.features.is_empty() {
        let obj = composed
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("composed config is not a JSON object"))?;
        let features = obj
            .entry("features")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(features_map) = features.as_object_mut() {
            for feature_ref in &recipe.features {
                // Don't overwrite existing feature options
                if !features_map.contains_key(feature_ref) {
                    features_map.insert(feature_ref.clone(), serde_json::json!({}));
                }
            }
        }
    }

    Ok(composed)
}

/// Compose the config and write it to the project's `.devcontainer/devcontainer.json`.
/// Also copies auxiliary files (Dockerfiles, compose files, scripts) from the
/// global template directory so that relative paths in the config resolve correctly.
/// Returns the path to the written file.
pub fn compose_and_write(recipe: &Recipe, runtime_name: &str) -> anyhow::Result<PathBuf> {
    let composed = compose_config(recipe, runtime_name)?;

    let folder_name = Path::new(&recipe.root_folder)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    let dest_dir = crate::util::paths::devcontainers_dir()
        .join(&folder_name)
        .join(".devcontainer");
    fs::create_dir_all(&dest_dir)?;

    // Copy auxiliary files (Dockerfiles, compose files, etc.) from the global template.
    let global_devcontainer_dir = global_dir()
        .join(&recipe.global_template)
        .join(".devcontainer");
    copy_auxiliary_files(&global_devcontainer_dir, &dest_dir, &recipe.options)?;

    let dest_path = dest_dir.join("devcontainer.json");
    let formatted = serde_json::to_string_pretty(&composed)?;
    fs::write(&dest_path, &formatted)?;

    Ok(dest_path)
}

/// Copy non-config files from a global template's `.devcontainer/` directory to
/// the destination directory, applying `${templateOption:...}` substitutions to
/// text files. Skips `devcontainer.json` and `devcontainer-template.json` since
/// those are handled by the composition pipeline.
fn copy_auxiliary_files(
    src_dir: &Path,
    dest_dir: &Path,
    options: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if !src_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "devcontainer.json" || name_str == "devcontainer-template.json" {
            continue;
        }
        let src_path = entry.path();
        let dest_path = dest_dir.join(&name);
        if src_path.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_auxiliary_files(&src_path, &dest_path, options)?;
        } else {
            match fs::read_to_string(&src_path) {
                Ok(content) => {
                    let substituted =
                        crate::devcontainer::templates::substitute_options(&content, options);
                    fs::write(&dest_path, substituted)?;
                }
                Err(_) => {
                    fs::copy(&src_path, &dest_path)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Set up a temp `~/.dev/` structure for testing. Returns (temp_dir, recipe).
    /// Caller must set env or use the returned paths directly.
    fn setup_test_env(
        global_config: &str,
        base_config: Option<&str>,
        runtime_config: Option<&str>,
        runtime_name: &str,
    ) -> (TempDir, Value) {
        let dir = TempDir::new().unwrap();
        let dev_home = dir.path();

        // Global template
        let global_dir = dev_home.join("global/test-lang/.devcontainer");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("devcontainer.json"), global_config).unwrap();

        // Base config
        if let Some(base) = base_config {
            let base_dir = dev_home.join("base");
            fs::create_dir_all(&base_dir).unwrap();
            fs::write(base_dir.join("devcontainer.json"), base).unwrap();
        }

        // Runtime config
        if let Some(rt) = runtime_config {
            let rt_dir = dev_home.join(runtime_name);
            fs::create_dir_all(&rt_dir).unwrap();
            fs::write(rt_dir.join("devcontainer.json"), rt).unwrap();
        }

        // Manually compose using the path helpers
        let global_path = dev_home.join("global/test-lang/.devcontainer/devcontainer.json");
        let global = read_json_file(&global_path).unwrap().unwrap();

        let base_path = dev_home.join("base/devcontainer.json");
        let base = read_json_file(&base_path).unwrap();

        let rt_path = dev_home.join(format!("{runtime_name}/devcontainer.json"));
        let runtime = read_json_file(&rt_path).unwrap();

        let mut layers = vec![global];
        if let Some(b) = base {
            layers.push(b);
        }
        if let Some(r) = runtime {
            layers.push(r);
        }

        let composed = merge_layers(&layers);
        (dir, composed)
    }

    #[test]
    fn test_compose_global_only() {
        let (_dir, composed) = setup_test_env(
            r#"{"image": "rust:latest", "remoteUser": "vscode"}"#,
            None,
            None,
            "docker",
        );
        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
    }

    #[test]
    fn test_compose_base_overrides_global() {
        let (_dir, composed) = setup_test_env(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );
        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
    }

    #[test]
    fn test_compose_runtime_overrides_all() {
        let (_dir, composed) = setup_test_env(
            r#"{"image": "rust:latest", "runArgs": ["--init"]}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            Some(r#"{"runArgs": ["--userns=keep-id"]}"#),
            "podman",
        );
        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
        let run_args = composed["runArgs"].as_array().unwrap();
        assert_eq!(run_args.len(), 2);
        assert!(run_args.contains(&Value::String("--init".to_string())));
        assert!(run_args.contains(&Value::String("--userns=keep-id".to_string())));
    }

    #[test]
    fn test_compose_features_merged() {
        let (_dir, composed) = setup_test_env(
            r#"{"image": "rust:latest", "features": {"ghcr.io/features/rust": {}}}"#,
            Some(r#"{"features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );
        let features = composed["features"].as_object().unwrap();
        assert!(features.contains_key("ghcr.io/features/rust"));
        assert!(features.contains_key("ghcr.io/features/zsh"));
    }

    #[test]
    fn test_substitute_template_options() {
        let mut value = serde_json::json!({
            "image": "python:${templateOption:imageVariant}",
            "remoteUser": "vscode"
        });
        let mut opts = HashMap::new();
        opts.insert("imageVariant".to_string(), "3.11".to_string());
        super::substitute_template_options(&mut value, &opts);
        assert_eq!(value["image"], "python:3.11");
        assert_eq!(value["remoteUser"], "vscode");
    }
}
