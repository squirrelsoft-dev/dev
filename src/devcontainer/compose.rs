use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::devcontainer::jsonc::parse_jsonc;
use crate::devcontainer::merge::{merge_layer, merge_layers};
use crate::devcontainer::recipe::{is_empty_object, Recipe};
use crate::util::paths::DevHome;

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
pub(crate) fn compose_config_with_base(
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<Value> {
    compose_config_in(&DevHome::current(), recipe, runtime_name, include_base)
}

pub(crate) fn compose_config_in(
    dev_home: &DevHome,
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<Value> {
    // Layer 1: Global template
    let global_config_path = dev_home.global_template_config(&recipe.global_template);
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
    let base = if include_base {
        read_json_file(&dev_home.base_config())?
    } else {
        None
    };

    // Layer 3: Runtime config
    let runtime = read_json_file(&dev_home.runtime_config(runtime_name))?;

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

    // Apply user customizations as the highest-priority layer
    if !is_empty_object(&recipe.customizations) {
        merge_layer(&mut composed, &recipe.customizations);
    }

    Ok(composed)
}

/// Compose the config and write it to the project's `.devcontainer/devcontainer.json`.
/// Also copies auxiliary files (Dockerfiles, compose files, scripts) from the
/// global template directory so that relative paths in the config resolve correctly.
/// Returns the path to the written file along with the composed value.
///
/// The persisted file is always the full, base-inclusive composition. A run that
/// opts out of the base layer composes its own config in memory via
/// [`compose_config_with_base`] instead, so `--no-base` cannot leave a base-free
/// config behind for the next `dev config` or `dev up` to read.
pub(crate) fn compose_and_write(
    recipe: &Recipe,
    runtime_name: &str,
) -> anyhow::Result<(PathBuf, Value)> {
    compose_and_write_in(&DevHome::current(), recipe, runtime_name)
}

pub(crate) fn compose_and_write_in(
    dev_home: &DevHome,
    recipe: &Recipe,
    runtime_name: &str,
) -> anyhow::Result<(PathBuf, Value)> {
    let composed = compose_config_in(dev_home, recipe, runtime_name, true)?;

    let folder_name = Path::new(&recipe.root_folder)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    let dest_dir = dev_home.project_devcontainer_dir(&folder_name);
    fs::create_dir_all(&dest_dir)?;

    // Copy auxiliary files (Dockerfiles, compose files, etc.) from the global template.
    let global_devcontainer_dir = dev_home.global_template_dir(&recipe.global_template);
    copy_auxiliary_files(&global_devcontainer_dir, &dest_dir, &recipe.options)?;

    let dest_path = dest_dir.join("devcontainer.json");
    let formatted = serde_json::to_string_pretty(&composed)?;
    fs::write(&dest_path, &formatted)?;

    Ok((dest_path, composed))
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

    /// Build a temp `~/.dev/` tree and a recipe pointing at the `test-lang`
    /// template in it. Composition then runs against this root through
    /// [`DevHome::at`], so the assertions below exercise the real production
    /// path rather than a re-implementation of it.
    struct TestDevHome {
        _dir: TempDir,
        dev_home: DevHome,
        workspace: PathBuf,
    }

    impl TestDevHome {
        fn new(
            global_config: &str,
            base_config: Option<&str>,
            runtime_config: Option<&str>,
            runtime_name: &str,
        ) -> Self {
            let dir = TempDir::new().unwrap();
            let root = dir.path().to_path_buf();

            let global_dir = root.join("global/test-lang/.devcontainer");
            fs::create_dir_all(&global_dir).unwrap();
            fs::write(global_dir.join("devcontainer.json"), global_config).unwrap();

            if let Some(base) = base_config {
                let base_dir = root.join("base");
                fs::create_dir_all(&base_dir).unwrap();
                fs::write(base_dir.join("devcontainer.json"), base).unwrap();
            }

            if let Some(rt) = runtime_config {
                let rt_dir = root.join(runtime_name);
                fs::create_dir_all(&rt_dir).unwrap();
                fs::write(rt_dir.join("devcontainer.json"), rt).unwrap();
            }

            let workspace = root.join("projects/demo");
            fs::create_dir_all(&workspace).unwrap();

            TestDevHome {
                dev_home: DevHome::at(root),
                workspace,
                _dir: dir,
            }
        }

        fn recipe(&self) -> Recipe {
            Recipe {
                global_template: "test-lang".to_string(),
                features: Vec::new(),
                options: HashMap::new(),
                root_folder: self.workspace.to_string_lossy().to_string(),
                customizations: serde_json::json!({}),
            }
        }

        fn compose(&self, recipe: &Recipe, runtime_name: &str, include_base: bool) -> Value {
            compose_config_in(&self.dev_home, recipe, runtime_name, include_base).unwrap()
        }

        fn persisted(&self) -> Value {
            let path = self
                .dev_home
                .project_devcontainer_dir("demo")
                .join("devcontainer.json");
            read_json_file(&path)
                .unwrap()
                .expect("composed config should be written")
        }
    }

    #[test]
    fn compose_global_only() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "vscode"}"#,
            None,
            None,
            "docker",
        );

        let composed = env.compose(&env.recipe(), "docker", true);

        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
    }

    #[test]
    fn compose_base_overrides_global() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );

        let composed = env.compose(&env.recipe(), "docker", true);

        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
    }

    #[test]
    fn compose_runtime_overrides_all() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "runArgs": ["--init"]}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            Some(r#"{"runArgs": ["--userns=keep-id"]}"#),
            "podman",
        );

        let composed = env.compose(&env.recipe(), "podman", true);

        assert_eq!(composed["image"], "rust:latest");
        assert_eq!(composed["remoteUser"], "vscode");
        let run_args = composed["runArgs"].as_array().unwrap();
        assert_eq!(run_args.len(), 2);
        assert!(run_args.contains(&Value::String("--init".to_string())));
        assert!(run_args.contains(&Value::String("--userns=keep-id".to_string())));
    }

    #[test]
    fn compose_features_merged() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "features": {"ghcr.io/features/rust": {}}}"#,
            Some(r#"{"features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );

        let composed = env.compose(&env.recipe(), "docker", true);

        let features = composed["features"].as_object().unwrap();
        assert!(features.contains_key("ghcr.io/features/rust"));
        assert!(features.contains_key("ghcr.io/features/zsh"));
    }

    #[test]
    fn compose_injects_recipe_features_and_customizations() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root", "forwardPorts": [3000]}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );
        let mut recipe = env.recipe();
        recipe.features = vec!["ghcr.io/features/node".to_string()];
        recipe.customizations = serde_json::json!({
            "remoteUser": "developer",
            "forwardPorts": [9090],
            "remoteEnv": {"MY_VAR": "hello"}
        });

        let composed = env.compose(&recipe, "docker", true);

        assert_eq!(composed["remoteUser"], "developer");
        assert!(composed["features"]
            .as_object()
            .unwrap()
            .contains_key("ghcr.io/features/node"));
        let ports = composed["forwardPorts"].as_array().unwrap();
        assert!(ports.contains(&Value::Number(3000.into())));
        assert!(ports.contains(&Value::Number(9090.into())));
        assert_eq!(composed["remoteEnv"]["MY_VAR"], "hello");
        assert_eq!(composed["image"], "rust:latest");
    }

    #[test]
    fn compose_applies_template_options() {
        let env = TestDevHome::new(
            r#"{"image": "python:${templateOption:imageVariant}", "remoteUser": "vscode"}"#,
            None,
            None,
            "docker",
        );
        let mut recipe = env.recipe();
        recipe
            .options
            .insert("imageVariant".to_string(), "3.11".to_string());

        let composed = env.compose(&recipe, "docker", true);

        assert_eq!(composed["image"], "python:3.11");
        assert_eq!(composed["remoteUser"], "vscode");
    }

    #[test]
    fn excluding_the_base_layer_drops_only_base_contributions() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode", "features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );

        let without_base = env.compose(&env.recipe(), "docker", false);

        assert_eq!(without_base["image"], "rust:latest");
        assert_eq!(without_base["remoteUser"], "root");
        assert!(without_base.get("features").is_none());
    }

    #[test]
    fn persisted_composition_always_includes_the_base_layer() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode", "features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();

        let (path, returned) = compose_and_write_in(&env.dev_home, &recipe, "docker").unwrap();

        assert_eq!(
            returned,
            env.persisted(),
            "returned value must match the file"
        );
        assert_eq!(returned["remoteUser"], "vscode");
        assert!(path.ends_with("demo/.devcontainer/devcontainer.json"));
    }

    #[test]
    fn composing_without_base_leaves_the_persisted_file_untouched() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode", "features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();
        compose_and_write_in(&env.dev_home, &recipe, "docker").unwrap();
        let before = env.persisted();

        let run_config = env.compose(&recipe, "docker", false);

        assert_eq!(
            run_config["remoteUser"], "root",
            "the run drops the base layer"
        );
        assert_eq!(
            env.persisted(),
            before,
            "--no-base must not rewrite the persisted composition"
        );
        assert_eq!(env.persisted()["remoteUser"], "vscode");
    }

    #[test]
    fn compose_and_write_copies_auxiliary_template_files() {
        let env = TestDevHome::new(
            r#"{"build": {"dockerfile": "Dockerfile"}}"#,
            None,
            None,
            "docker",
        );
        let aux = env
            .dev_home
            .global_template_dir("test-lang")
            .join("Dockerfile");
        fs::write(&aux, "FROM ${templateOption:base}\n").unwrap();
        let mut recipe = env.recipe();
        recipe
            .options
            .insert("base".to_string(), "rust:latest".to_string());

        compose_and_write_in(&env.dev_home, &recipe, "docker").unwrap();

        let copied = env
            .dev_home
            .project_devcontainer_dir("demo")
            .join("Dockerfile");
        assert_eq!(fs::read_to_string(copied).unwrap(), "FROM rust:latest\n");
    }

    #[test]
    fn missing_global_template_names_the_path() {
        let env = TestDevHome::new(r#"{"image": "rust:latest"}"#, None, None, "docker");
        let mut recipe = env.recipe();
        recipe.global_template = "absent".to_string();

        let err = compose_config_in(&env.dev_home, &recipe, "docker", true).unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("absent"),
            "should name the template: {message}"
        );
        assert!(
            message.contains("global/absent"),
            "should name the searched path: {message}"
        );
    }
}
