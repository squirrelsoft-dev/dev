use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::error::DevError;
use crate::util::paths::base_config_dir;

use super::config::DevcontainerConfig;
use super::features::{ResolvedFeature, features_required_by};
use super::jsonc::parse_jsonc;
use super::lockfile::{handle_lockfile, lockfile_path};
use super::merge::merge_layers;

/// A project configuration with the base layer merged in, plus the provenance
/// needed to keep base-contributed state out of project-owned artifacts.
pub(crate) struct EffectiveConfig {
    pub(crate) config: DevcontainerConfig,
    /// Feature ids the base layer contributed that the project does not declare.
    pub(crate) base_feature_ids: HashSet<String>,
}

pub(crate) fn load_effective_config(
    config_path: &Path,
    include_base: bool,
) -> anyhow::Result<EffectiveConfig> {
    let base_path = base_config_dir().join("devcontainer.json");
    let (value, base_feature_ids) =
        load_effective_config_value(config_path, include_base, &base_path)?;
    let config = serde_json::from_value(value)
        .map_err(|e| DevError::InvalidConfig(format!("Failed to parse merged config: {e}")))?;
    Ok(EffectiveConfig {
        config,
        base_feature_ids,
    })
}

/// Build an [`EffectiveConfig`] from an already-composed config value.
///
/// Recipe projects compose their layers up front, so the base layer is either
/// already baked in or was deliberately left out; either way there is nothing
/// further to merge. `base_feature_ids` still carries the base's own feature ids
/// so they stay out of the project's lockfile.
pub(crate) fn effective_config_from_parts(
    value: Value,
    base_feature_ids: HashSet<String>,
) -> anyhow::Result<EffectiveConfig> {
    let config = serde_json::from_value(value)
        .map_err(|e| DevError::InvalidConfig(format!("Failed to parse composed config: {e}")))?;
    Ok(EffectiveConfig {
        config,
        base_feature_ids,
    })
}

/// Everything needed to decide what a run may write to a project's lockfile.
///
/// The rule — exclude base-contributed features — applies identically to every
/// build path in `dev up` and `dev build`, so it lives here rather than being
/// restated at each `handle_lockfile` call.
pub(crate) struct LockfilePolicy {
    pub(crate) base_feature_ids: HashSet<String>,
    pub(crate) frozen: bool,
}

impl LockfilePolicy {
    pub(crate) fn new(effective: &EffectiveConfig, frozen: bool) -> Self {
        LockfilePolicy {
            base_feature_ids: effective.base_feature_ids.clone(),
            frozen,
        }
    }

    /// Reconcile the lockfile in `devcontainer_dir` against this run's features.
    pub(crate) fn apply(
        &self,
        devcontainer_dir: Option<&Path>,
        features: &[ResolvedFeature],
    ) -> anyhow::Result<()> {
        let Some(dir) = devcontainer_dir else {
            return Ok(());
        };
        let owned = project_owned_features(features, &self.base_feature_ids);
        if owned.is_empty() {
            return Ok(());
        }
        handle_lockfile(&lockfile_path(dir), &owned, self.frozen)?;
        Ok(())
    }
}

/// Features that belong to the project: everything it declares, plus the
/// dependency closure of those declarations. Base-contributed features and any
/// dependency pulled in only by them are excluded.
pub(crate) fn project_owned_features(
    features: &[ResolvedFeature],
    base_feature_ids: &HashSet<String>,
) -> Vec<ResolvedFeature> {
    if base_feature_ids.is_empty() {
        return features.to_vec();
    }
    let roots: HashSet<String> = features
        .iter()
        .filter(|f| !f.is_dependency && !base_feature_ids.contains(&f.id))
        .map(|f| f.id.clone())
        .collect();
    let owned = features_required_by(features, &roots);
    features
        .iter()
        .filter(|f| owned.contains(&f.id))
        .cloned()
        .collect()
}

pub(crate) fn load_effective_config_value(
    config_path: &Path,
    include_base: bool,
    base_config_path: &Path,
) -> anyhow::Result<(Value, HashSet<String>)> {
    let mut layers = Vec::new();
    let mut base_feature_ids = HashSet::new();
    if include_base && base_config_path.is_file() {
        let mut base = read_json_file(base_config_path)?;
        if !base.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            if let Some(base_dir) = base_config_path.parent() {
                absolutize_config_paths(&mut base, base_dir);
            }
            base_feature_ids = declared_feature_ids(&base);
            layers.push(base);
        }
    }

    let project = read_json_file(config_path)?;
    let project_definition = config_definition(&project);
    for id in declared_feature_ids(&project) {
        base_feature_ids.remove(&id);
    }
    layers.push(project);

    let mut merged = merge_layers(&layers);
    prune_lower_priority_definitions(&mut merged, project_definition);
    Ok((merged, base_feature_ids))
}

fn declared_feature_ids(value: &Value) -> HashSet<String> {
    value
        .get("features")
        .and_then(Value::as_object)
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// Rewrite relative paths in an overlay config so they resolve against that
/// config's own directory rather than the effective config directory.
///
/// Runtime/base layers are merged in memory beneath a project config that lives
/// somewhere else entirely, so every downstream consumer (Dockerfile reads, bind
/// mount sources, local feature lookups) would otherwise resolve their paths
/// against the wrong root and fail with a bare "no such file or directory".
pub(crate) fn absolutize_config_paths(base: &mut Value, base_dir: &Path) {
    let Some(obj) = base.as_object_mut() else {
        return;
    };

    if let Some(build) = obj.get_mut("build").and_then(Value::as_object_mut) {
        for key in ["dockerfile", "context"] {
            if let Some(Value::String(path)) = build.get(key)
                && let Some(abs) = resolve_against_base(path, base_dir)
            {
                build.insert(key.to_string(), Value::String(abs));
            }
        }
    }

    match obj.get_mut("dockerComposeFile") {
        Some(Value::String(path)) => {
            if let Some(abs) = resolve_against_base(path, base_dir) {
                obj.insert("dockerComposeFile".to_string(), Value::String(abs));
            }
        }
        Some(Value::Array(paths)) => {
            for entry in paths.iter_mut() {
                if let Value::String(path) = entry
                    && let Some(abs) = resolve_against_base(path, base_dir)
                {
                    *entry = Value::String(abs);
                }
            }
        }
        _ => {}
    }

    if let Some(Value::Object(features)) = obj.get("features") {
        let rebased: serde_json::Map<String, Value> = features
            .iter()
            .map(|(id, options)| {
                let key = resolve_local_ref(id, base_dir).unwrap_or_else(|| id.clone());
                (key, options.clone())
            })
            .collect();
        obj.insert("features".to_string(), Value::Object(rebased));
    }

    if let Some(Value::Array(mounts)) = obj.get_mut("mounts") {
        for mount in mounts.iter_mut() {
            match mount {
                Value::String(spec) => *spec = absolutize_mount_string(spec, base_dir),
                Value::Object(fields) => {
                    if let Some(Value::String(source)) = fields.get("source")
                        && let Some(abs) = resolve_local_ref(source, base_dir)
                    {
                        fields.insert("source".to_string(), Value::String(abs));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Absolutize a relative path against the base config directory. Returns `None`
/// for absolute paths and for values carrying a `${...}` variable, which is
/// substituted later against the workspace.
fn resolve_against_base(value: &str, base_dir: &Path) -> Option<String> {
    if value.contains("${") || !Path::new(value).is_relative() {
        return None;
    }
    Some(base_dir.join(value).to_string_lossy().into_owned())
}

/// Absolutize only explicitly-relative (`./`, `../`) references. Used where a
/// bare name means something other than a path — an OCI feature id, or a named
/// volume in a mount source.
fn resolve_local_ref(value: &str, base_dir: &Path) -> Option<String> {
    if !value.starts_with("./") && !value.starts_with("../") {
        return None;
    }
    resolve_against_base(value, base_dir)
}

/// Rebase the source of a mount entry, in either the short (`./src:/dst[:ro]`)
/// or long (`source=./src,target=/dst`) form that `parse_single_mount` accepts.
fn absolutize_mount_string(spec: &str, base_dir: &Path) -> String {
    let trimmed = spec.trim_start();
    if trimmed.starts_with("./") || trimmed.starts_with("../") {
        let Some((source, rest)) = trimmed.split_once(':') else {
            return spec.to_string();
        };
        return match resolve_local_ref(source, base_dir) {
            Some(abs) => format!("{abs}:{rest}"),
            None => spec.to_string(),
        };
    }

    spec.split(',')
        .map(|part| {
            let Some((key, value)) = part.split_once('=') else {
                return part.to_string();
            };
            if matches!(key.trim(), "source" | "src")
                && let Some(abs) = resolve_local_ref(value, base_dir)
            {
                return format!("{key}={abs}");
            }
            part.to_string()
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn read_json_file(path: &Path) -> anyhow::Result<Value> {
    let raw = fs::read_to_string(path)
        .map_err(|e| DevError::InvalidConfig(format!("Failed to read {}: {e}", path.display())))?;
    parse_jsonc(&raw).map_err(Into::into)
}

#[derive(Clone, Copy)]
pub(crate) enum ConfigDefinition {
    Image,
    Build,
    Compose,
}

pub(crate) fn config_definition(value: &Value) -> Option<ConfigDefinition> {
    let obj = value.as_object()?;
    if obj.contains_key("dockerComposeFile") {
        Some(ConfigDefinition::Compose)
    } else if obj.contains_key("build") {
        Some(ConfigDefinition::Build)
    } else if obj.contains_key("image") {
        Some(ConfigDefinition::Image)
    } else {
        None
    }
}

pub(crate) fn prune_lower_priority_definitions(
    merged: &mut Value,
    project_definition: Option<ConfigDefinition>,
) {
    let Some(obj) = merged.as_object_mut() else {
        return;
    };
    match project_definition {
        Some(ConfigDefinition::Image) => {
            obj.remove("build");
            obj.remove("dockerComposeFile");
            obj.remove("service");
        }
        Some(ConfigDefinition::Build) => {
            obj.remove("image");
            obj.remove("dockerComposeFile");
            obj.remove("service");
        }
        Some(ConfigDefinition::Compose) => {
            obj.remove("image");
            obj.remove("build");
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        absolutize_mount_string, effective_config_from_parts, load_effective_config_value,
        project_owned_features,
    };
    use crate::devcontainer::config::{DevcontainerConfig, LifecycleCommand};
    use crate::devcontainer::features::{ResolvedFeature, feature_image_tag};
    use crate::devcontainer::resolve_features;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_project_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let devcontainer_dir = dir.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let path = devcontainer_dir.join("devcontainer.json");
        fs::write(&path, content).unwrap();
        path
    }

    fn write_base_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let base_dir = dir.path().join("base");
        fs::create_dir_all(&base_dir).unwrap();
        let path = base_dir.join("devcontainer.json");
        fs::write(&path, content).unwrap();
        path
    }

    fn load_config_with_base(
        config_path: &Path,
        include_base: bool,
        base_config_path: &Path,
    ) -> DevcontainerConfig {
        let (value, _) = load_effective_config_value(config_path, include_base, base_config_path)
            .expect("effective config should load");
        serde_json::from_value(value).expect("effective config should deserialize")
    }

    fn effective_value(
        config_path: &Path,
        include_base: bool,
        base_config_path: &Path,
    ) -> serde_json::Value {
        load_effective_config_value(config_path, include_base, base_config_path)
            .expect("effective config should load")
            .0
    }

    fn base_feature_ids(
        config_path: &Path,
        include_base: bool,
        base_config_path: &Path,
    ) -> std::collections::HashSet<String> {
        load_effective_config_value(config_path, include_base, base_config_path)
            .expect("effective config should load")
            .1
    }

    fn make_test_feature(id: &str, is_dependency: bool) -> ResolvedFeature {
        ResolvedFeature {
            id: id.to_string(),
            oci_ref: id.to_string(),
            version: "1".to_string(),
            options: serde_json::Value::Null,
            install_script_path: std::path::PathBuf::new(),
            install_after: Vec::new(),
            container_env: HashMap::new(),
            mounts: Vec::new(),
            init: false,
            privileged: false,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            entrypoint: None,
            lifecycle_hooks: Default::default(),
            is_dependency,
        }
    }

    #[test]
    fn image_config_applies_base_in_memory_beneath_project() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "features": {
                    "ghcr.io/devcontainers/features/node:1": {"version": "22"}
                },
                "containerEnv": {
                    "SHARED": "project"
                }
            }"#,
        );
        let original = fs::read_to_string(&config_path).unwrap();
        let base_path = write_base_config(
            &home,
            r#"{
                "build": {
                    "dockerfile": "Base.Dockerfile"
                },
                "features": {
                    "ghcr.io/devcontainers/features/github-cli:1": {}
                },
                "containerEnv": {
                    "EDITOR": "nvim",
                    "SHARED": "base"
                }
            }"#,
        );

        let config = load_config_with_base(&config_path, true, &base_path);

        assert_eq!(config.image.as_deref(), Some("ubuntu:24.04"));
        assert!(config.build.is_none());
        let features = config.features.unwrap();
        assert!(features.contains_key("ghcr.io/devcontainers/features/node:1"));
        assert!(features.contains_key("ghcr.io/devcontainers/features/github-cli:1"));
        let env = config.container_env.unwrap();
        assert_eq!(env["EDITOR"], "nvim");
        assert_eq!(env["SHARED"], "project");
        assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    }

    #[test]
    fn build_config_applies_base_without_changing_config_type() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "build": {
                    "dockerfile": "Dockerfile",
                    "context": "."
                },
                "remoteEnv": {
                    "RUST_LOG": "debug"
                }
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{
                "image": "base:latest",
                "features": {
                    "ghcr.io/devcontainers/features/rust:1": {}
                },
                "remoteEnv": {
                    "EDITOR": "nvim"
                }
            }"#,
        );

        let config = load_config_with_base(&config_path, true, &base_path);

        assert!(config.build.is_some());
        assert!(config.docker_compose_file.is_none());
        assert_eq!(config.image, None);
        assert!(
            config
                .features
                .unwrap()
                .contains_key("ghcr.io/devcontainers/features/rust:1")
        );
        let env = config.remote_env.unwrap();
        assert_eq!(env["EDITOR"], "nvim");
        assert_eq!(env["RUST_LOG"], "debug");
    }

    #[test]
    fn compose_config_applies_base_without_changing_config_type() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "dockerComposeFile": "compose.yml",
                "service": "app",
                "containerEnv": {
                    "APP_ENV": "dev"
                }
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{
                "image": "base:latest",
                "build": {
                    "dockerfile": "Base.Dockerfile"
                },
                "features": {
                    "ghcr.io/devcontainers/features/github-cli:1": {}
                },
                "containerEnv": {
                    "EDITOR": "nvim"
                }
            }"#,
        );

        let config = load_config_with_base(&config_path, true, &base_path);

        assert!(config.is_compose());
        assert!(config.image.is_none());
        assert!(config.build.is_none());
        assert_eq!(config.service.as_deref(), Some("app"));
        assert!(
            config
                .features
                .unwrap()
                .contains_key("ghcr.io/devcontainers/features/github-cli:1")
        );
        let env = config.container_env.unwrap();
        assert_eq!(env["APP_ENV"], "dev");
        assert_eq!(env["EDITOR"], "nvim");
    }

    #[test]
    fn project_config_takes_precedence_over_base() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "project:latest",
                "remoteUser": "project",
                "containerEnv": {
                    "SHARED": "project"
                },
                "features": {
                    "ghcr.io/devcontainers/features/rust:1": {"profile": "project"}
                }
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{
                "image": "base:latest",
                "remoteUser": "base",
                "containerEnv": {
                    "SHARED": "base",
                    "EDITOR": "nvim"
                },
                "features": {
                    "ghcr.io/devcontainers/features/rust:1": {"profile": "base"}
                }
            }"#,
        );

        let value = effective_value(&config_path, true, &base_path);

        assert_eq!(value["image"], "project:latest");
        assert_eq!(value["remoteUser"], "project");
        assert_eq!(value["containerEnv"]["SHARED"], "project");
        assert_eq!(value["containerEnv"]["EDITOR"], "nvim");
        assert_eq!(
            value["features"]["ghcr.io/devcontainers/features/rust:1"]["profile"],
            "project"
        );
    }

    #[test]
    fn absent_base_is_a_noop() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{"image": "ubuntu:24.04", "containerEnv": {"APP_ENV": "dev"}}"#,
        );
        let missing_base = home.path().join("base/devcontainer.json");

        let value = effective_value(&config_path, true, &missing_base);

        assert_eq!(value["image"], "ubuntu:24.04");
        assert_eq!(value["containerEnv"]["APP_ENV"], "dev");
        assert!(value.get("features").is_none());
    }

    #[test]
    fn no_base_skips_existing_base_config() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(&workspace, r#"{"image": "ubuntu:24.04"}"#);
        let base_path = write_base_config(
            &home,
            r#"{"containerEnv": {"EDITOR": "nvim"}, "features": {"ghcr.io/features/gh": {}}}"#,
        );

        let value = effective_value(&config_path, false, &base_path);

        assert_eq!(value["image"], "ubuntu:24.04");
        assert!(value.get("containerEnv").is_none());
        assert!(value.get("features").is_none());
    }

    #[test]
    fn named_lifecycle_commands_from_base_and_project_union() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "postCreateCommand": {
                    "project": "cargo fetch",
                    "shared": "project"
                }
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{
                "postCreateCommand": {
                    "dotfiles": "install-dotfiles",
                    "shared": "base"
                }
            }"#,
        );

        let config = load_config_with_base(&config_path, true, &base_path);

        let LifecycleCommand::Parallel(commands) = config.post_create_command.unwrap() else {
            panic!("postCreateCommand should deserialize as named parallel commands");
        };
        assert_eq!(commands["dotfiles"], "install-dotfiles");
        assert_eq!(commands["project"], "cargo fetch");
        assert_eq!(commands["shared"], "project");
    }

    #[test]
    fn feature_image_tag_differs_when_base_contributes_features() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "features": {"ghcr.io/devcontainers/features/node:1": {}}
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{"features": {"ghcr.io/devcontainers/features/github-cli:1": {}}}"#,
        );

        let with_base = load_config_with_base(&config_path, true, &base_path);
        let without_base = load_config_with_base(&config_path, false, &base_path);

        let with_base_tag = feature_image_tag(
            "vsc-demo",
            &with_base,
            &crate::devcontainer::resolve_features(&with_base).unwrap(),
        );
        let without_base_tag = feature_image_tag(
            "vsc-demo",
            &without_base,
            &crate::devcontainer::resolve_features(&without_base).unwrap(),
        );

        assert_ne!(
            with_base_tag, without_base_tag,
            "a cached image built without the base features must not look like a cache hit"
        );
        assert!(with_base_tag.starts_with("vsc-demo-features-"));
    }

    #[test]
    fn feature_image_tag_is_stable_for_identical_configs() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "features": {"ghcr.io/devcontainers/features/node:1": {"version": "22"}}
            }"#,
        );
        let missing_base = home.path().join("base/devcontainer.json");

        let config = load_config_with_base(&config_path, true, &missing_base);
        let features = crate::devcontainer::resolve_features(&config).unwrap();

        assert_eq!(
            feature_image_tag("vsc-demo", &config, &features),
            feature_image_tag("vsc-demo", &config, &features)
        );
    }

    #[test]
    fn feature_image_tag_changes_when_the_image_changes() {
        let workspace = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let missing_base = home.path().join("base/devcontainer.json");
        let a = write_project_config(
            &workspace,
            r#"{"image": "ubuntu:24.04", "features": {"ghcr.io/f/node:1": {}}}"#,
        );
        let b = write_project_config(
            &other,
            r#"{"image": "debian:12", "features": {"ghcr.io/f/node:1": {}}}"#,
        );

        let config_a = load_config_with_base(&a, true, &missing_base);
        let config_b = load_config_with_base(&b, true, &missing_base);

        assert_ne!(
            feature_image_tag(
                "vsc-demo",
                &config_a,
                &crate::devcontainer::resolve_features(&config_a).unwrap()
            ),
            feature_image_tag(
                "vsc-demo",
                &config_b,
                &crate::devcontainer::resolve_features(&config_b).unwrap()
            )
        );
    }

    #[test]
    fn base_only_features_are_reported_as_base_owned() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "features": {"ghcr.io/f/node:1": {}, "ghcr.io/f/shared:1": {}}
            }"#,
        );
        let base_path = write_base_config(
            &home,
            r#"{"features": {"ghcr.io/f/gh:1": {}, "ghcr.io/f/shared:1": {}}}"#,
        );

        let ids = base_feature_ids(&config_path, true, &base_path);

        assert!(ids.contains("ghcr.io/f/gh:1"));
        assert!(
            !ids.contains("ghcr.io/f/shared:1"),
            "a feature the project also declares is project-owned"
        );
        assert!(!ids.contains("ghcr.io/f/node:1"));
        assert!(base_feature_ids(&config_path, false, &base_path).is_empty());
    }

    #[test]
    fn project_owned_features_excludes_base_features() {
        let features = vec![
            make_test_feature("ghcr.io/f/node:1", false),
            make_test_feature("ghcr.io/f/gh:1", false),
            make_test_feature("ghcr.io/f/common:1", true),
        ];
        let base_ids: std::collections::HashSet<String> =
            ["ghcr.io/f/gh:1".to_string()].into_iter().collect();

        let owned = project_owned_features(&features, &base_ids);
        let ids: Vec<&str> = owned.iter().map(|f| f.id.as_str()).collect();

        assert!(ids.contains(&"ghcr.io/f/node:1"));
        assert!(!ids.contains(&"ghcr.io/f/gh:1"));
        assert!(
            !ids.contains(&"ghcr.io/f/common:1"),
            "a dependency reachable only from a base feature stays out of the project lockfile"
        );
    }

    #[test]
    fn project_owned_features_is_identity_without_a_base_layer() {
        let features = vec![
            make_test_feature("ghcr.io/f/node:1", false),
            make_test_feature("ghcr.io/f/common:1", true),
        ];

        let owned = project_owned_features(&features, &std::collections::HashSet::new());

        assert_eq!(owned.len(), 2);
    }

    #[test]
    fn base_relative_paths_resolve_against_the_base_directory() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(&workspace, r#"{"containerEnv": {"A": "b"}}"#);
        let base_path = write_base_config(
            &home,
            r#"{
                "build": {"dockerfile": "Base.Dockerfile", "context": "."},
                "features": {"./local-feature": {}, "ghcr.io/f/gh:1": {}},
                "mounts": [
                    "source=./cache,target=/cache,type=bind",
                    "source=named-volume,target=/data,type=volume"
                ]
            }"#,
        );
        let base_dir = base_path.parent().unwrap();

        let value = effective_value(&config_path, true, &base_path);

        assert_eq!(
            value["build"]["dockerfile"],
            base_dir.join("Base.Dockerfile").to_string_lossy().as_ref()
        );
        assert_eq!(
            value["build"]["context"],
            base_dir.join(".").to_string_lossy().as_ref()
        );
        let features = value["features"].as_object().unwrap();
        assert!(features.contains_key(base_dir.join("./local-feature").to_string_lossy().as_ref()));
        assert!(
            features.contains_key("ghcr.io/f/gh:1"),
            "OCI references must not be treated as paths"
        );
        let mounts = value["mounts"].as_array().unwrap();
        assert_eq!(
            mounts[0],
            format!(
                "source={},target=/cache,type=bind",
                base_dir.join("./cache").to_string_lossy()
            )
        );
        assert_eq!(
            mounts[1], "source=named-volume,target=/data,type=volume",
            "named volumes must not be rewritten into paths"
        );
    }

    #[test]
    fn base_paths_with_variables_are_left_for_later_substitution() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(&workspace, r#"{"image": "ubuntu:24.04"}"#);
        let base_path = write_base_config(
            &home,
            r#"{"mounts": ["source=${localWorkspaceFolder}/.cache,target=/cache,type=bind"]}"#,
        );

        let value = effective_value(&config_path, true, &base_path);

        assert_eq!(
            value["mounts"][0],
            "source=${localWorkspaceFolder}/.cache,target=/cache,type=bind"
        );
    }

    #[test]
    fn short_form_base_mounts_are_rebased() {
        let base_dir = Path::new("/home/user/.dev/base");

        assert_eq!(
            absolutize_mount_string("./cache:/cache", base_dir),
            "/home/user/.dev/base/./cache:/cache"
        );
        assert_eq!(
            absolutize_mount_string("../shared:/shared:ro", base_dir),
            "/home/user/.dev/base/../shared:/shared:ro"
        );
    }

    #[test]
    fn short_form_mounts_with_absolute_or_variable_sources_are_untouched() {
        let base_dir = Path::new("/home/user/.dev/base");

        assert_eq!(
            absolutize_mount_string("/host/cache:/cache", base_dir),
            "/host/cache:/cache"
        );
        assert_eq!(
            absolutize_mount_string("${localWorkspaceFolder}/.cache:/cache", base_dir),
            "${localWorkspaceFolder}/.cache:/cache"
        );
        assert_eq!(
            absolutize_mount_string("named-volume:/data", base_dir),
            "named-volume:/data"
        );
    }

    #[test]
    fn short_form_base_mounts_survive_a_full_merge() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(&workspace, r#"{"image": "ubuntu:24.04"}"#);
        let base_path = write_base_config(&home, r#"{"mounts": ["./cache:/cache"]}"#);
        let base_dir = base_path.parent().unwrap();

        let value = effective_value(&config_path, true, &base_path);

        assert_eq!(
            value["mounts"][0],
            format!("{}:/cache", base_dir.join("./cache").to_string_lossy())
        );
    }

    #[test]
    fn tag_changes_when_base_contributes_remote_user() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{"image": "ubuntu:24.04", "features": {"ghcr.io/f/node:1": {}}}"#,
        );
        let base_path = write_base_config(&home, r#"{"remoteUser": "vscode"}"#);

        let with_base = load_config_with_base(&config_path, true, &base_path);
        let without_base = load_config_with_base(&config_path, false, &base_path);

        assert_eq!(with_base.remote_user.as_deref(), Some("vscode"));
        assert_ne!(
            feature_image_tag(
                "vsc-demo",
                &with_base,
                &resolve_features(&with_base).unwrap()
            ),
            feature_image_tag(
                "vsc-demo",
                &without_base,
                &resolve_features(&without_base).unwrap()
            ),
        );
    }

    #[test]
    fn tag_changes_when_base_contributes_env() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{"image": "ubuntu:24.04", "features": {"ghcr.io/f/node:1": {}}}"#,
        );
        let container_env_base =
            write_base_config(&home, r#"{"containerEnv": {"EDITOR": "nvim"}}"#);

        let plain = load_config_with_base(&config_path, false, &container_env_base);
        let with_container_env = load_config_with_base(&config_path, true, &container_env_base);

        let remote_env_home = TempDir::new().unwrap();
        let remote_env_base =
            write_base_config(&remote_env_home, r#"{"remoteEnv": {"EDITOR": "nvim"}}"#);
        let with_remote_env = load_config_with_base(&config_path, true, &remote_env_base);

        let tag = |config: &DevcontainerConfig| {
            feature_image_tag("vsc-demo", config, &resolve_features(config).unwrap())
        };

        assert_ne!(tag(&plain), tag(&with_container_env));
        assert_ne!(tag(&plain), tag(&with_remote_env));
        assert_ne!(tag(&with_container_env), tag(&with_remote_env));
    }

    #[test]
    fn tag_ignores_fields_that_never_reach_the_image() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let missing_base = home.path().join("base/devcontainer.json");
        let plain = write_project_config(
            &a,
            r#"{"image": "ubuntu:24.04", "features": {"ghcr.io/f/node:1": {}}}"#,
        );
        let with_ports = write_project_config(
            &b,
            r#"{
                "name": "renamed",
                "image": "ubuntu:24.04",
                "forwardPorts": [3000],
                "features": {"ghcr.io/f/node:1": {}}
            }"#,
        );

        let config_a = load_config_with_base(&plain, true, &missing_base);
        let config_b = load_config_with_base(&with_ports, true, &missing_base);

        assert_eq!(
            feature_image_tag("vsc-demo", &config_a, &resolve_features(&config_a).unwrap()),
            feature_image_tag("vsc-demo", &config_b, &resolve_features(&config_b).unwrap()),
        );
    }

    #[test]
    fn tag_is_insensitive_to_env_map_ordering() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let missing_base = home.path().join("base/devcontainer.json");
        let first = write_project_config(
            &a,
            r#"{
                "image": "ubuntu:24.04",
                "features": {"ghcr.io/f/node:1": {}},
                "containerEnv": {"A": "1", "B": "2"}
            }"#,
        );
        let second = write_project_config(
            &b,
            r#"{
                "image": "ubuntu:24.04",
                "features": {"ghcr.io/f/node:1": {}},
                "containerEnv": {"B": "2", "A": "1"}
            }"#,
        );

        let config_a = load_config_with_base(&first, true, &missing_base);
        let config_b = load_config_with_base(&second, true, &missing_base);

        assert_eq!(
            feature_image_tag("vsc-demo", &config_a, &resolve_features(&config_a).unwrap()),
            feature_image_tag("vsc-demo", &config_b, &resolve_features(&config_b).unwrap()),
        );
    }

    #[test]
    fn composed_config_contributes_no_base_owned_features() {
        let composed = serde_json::json!({
            "image": "rust:latest",
            "remoteUser": "vscode",
            "features": {
                "ghcr.io/f/node:1": {},
                "ghcr.io/f/gh:1": {}
            }
        });

        let effective = effective_config_from_parts(composed, HashSet::new()).unwrap();

        assert_eq!(effective.config.image.as_deref(), Some("rust:latest"));
        assert_eq!(effective.config.remote_user.as_deref(), Some("vscode"));
        assert!(
            effective.base_feature_ids.is_empty(),
            "a recipe composes its own layers, so nothing is withheld from its lockfile"
        );

        let features = resolve_features(&effective.config).unwrap();
        assert_eq!(
            project_owned_features(&features, &effective.base_feature_ids).len(),
            2
        );
    }

    #[test]
    fn composed_config_reports_parse_errors() {
        let Err(err) =
            effective_config_from_parts(serde_json::json!({"image": 42}), HashSet::new())
        else {
            panic!("a non-string image must not deserialize");
        };

        assert!(
            err.to_string().contains("composed config"),
            "error should name the composed config: {err}"
        );
    }
}
