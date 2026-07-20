use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::devcontainer::config::DevcontainerConfig;
use crate::devcontainer::effective::{
    absolutize_config_paths, config_definition, prune_lower_priority_definitions,
};
use crate::devcontainer::jsonc::parse_jsonc;
use crate::devcontainer::merge::{merge_layer, merge_layers};
use crate::devcontainer::recipe::{Recipe, is_empty_object};
use crate::error::DevError;
use crate::util::paths::DevHome;
use crate::util::workspace::{ConfigSource, find_config_source_in};

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
#[cfg(test)]
pub(crate) fn compose_config_in(
    dev_home: &DevHome,
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<Value> {
    Ok(compose_config_details_in(dev_home, recipe, runtime_name, include_base)?.value)
}

pub(crate) struct RecipeConfig {
    /// The devcontainer path used for relative-path resolution and container labels.
    ///
    /// Recipe projects no longer persist this file; it is a stable virtual path
    /// beside `recipe.json` so existing labels and lockfile locations remain
    /// compatible with the older composed-file flow.
    pub(crate) config_path: PathBuf,
    pub(crate) value: Value,
    pub(crate) base_feature_ids: HashSet<String>,
}

pub(crate) fn compose_recipe_config(
    recipe_path: &Path,
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<RecipeConfig> {
    compose_recipe_config_in(
        &DevHome::current(),
        recipe_path,
        recipe,
        runtime_name,
        include_base,
    )
}

pub(crate) fn compose_recipe_config_in(
    dev_home: &DevHome,
    recipe_path: &Path,
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<RecipeConfig> {
    let recipe_dir = recipe_dir_of(recipe_path)?;
    let details = compose_config_details_in(dev_home, recipe, runtime_name, include_base)?;
    Ok(RecipeConfig {
        config_path: recipe_dir.join("devcontainer.json"),
        value: details.value,
        base_feature_ids: details.base_feature_ids,
    })
}

/// Load a workspace's effective devcontainer config for read-only consumers such
/// as `dev shell` and `dev exec`.
///
/// Recipe projects keep no `devcontainer.json` on disk, so resolving them by path
/// alone yields nothing and callers silently fall back to the image's defaults.
/// Composing here keeps `remoteUser` and `workspaceFolder` identical to the ones
/// `dev up` built the container with.
pub(crate) fn load_workspace_config(
    workspace: &Path,
    runtime_name: &str,
) -> anyhow::Result<(PathBuf, DevcontainerConfig)> {
    load_workspace_config_in(&DevHome::current(), workspace, runtime_name)
}

pub(crate) fn load_workspace_config_in(
    dev_home: &DevHome,
    workspace: &Path,
    runtime_name: &str,
) -> anyhow::Result<(PathBuf, DevcontainerConfig)> {
    match find_config_source_in(dev_home, workspace)? {
        ConfigSource::Direct(path) => {
            let config = DevcontainerConfig::from_path(&path)?;
            Ok((path, config))
        }
        ConfigSource::Recipe(recipe_path) => {
            let recipe = Recipe::from_path(&recipe_path)?;
            let composed =
                compose_recipe_config_in(dev_home, &recipe_path, &recipe, runtime_name, true)?;
            let config = serde_json::from_value(composed.value)?;
            Ok((composed.config_path, config))
        }
    }
}

/// Load a workspace's config where a failure degrades behaviour rather than
/// stopping the command — `dev shell`, `dev exec`, `dev down`.
///
/// A workspace with no config at all is ordinary: a container can outlive the
/// config that created it. Any other failure is actionable, and the warning is
/// the only thing that explains why the command behaved as if there were no
/// config — a session on the wrong user, or a teardown that skipped `compose down`.
pub(crate) fn load_workspace_config_or_warn(
    workspace: &Path,
    runtime_name: &str,
) -> Option<(PathBuf, DevcontainerConfig)> {
    match load_workspace_config(workspace, runtime_name) {
        Ok(loaded) => Some(loaded),
        Err(err) => {
            if !matches!(err.downcast_ref::<DevError>(), Some(DevError::NoConfig(_))) {
                eprintln!("Warning: proceeding without the devcontainer config — {err}");
            }
            None
        }
    }
}

fn recipe_dir_of(recipe_path: &Path) -> anyhow::Result<&Path> {
    recipe_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("recipe path has no parent: {}", recipe_path.display()))
}

/// Put the template's auxiliary files (Dockerfiles, compose files, scripts) beside
/// a recipe so a build has the context it references.
///
/// Composition is read-only, so the commands that actually build — `dev up` and
/// `dev build` — call this first; `dev config` and the read-only config lookups
/// compose without it and never touch the project directory.
///
/// The returned manifest is deliberately dropped. `recipe.json` is the project's
/// durable state and only `dev new` may rewrite it, so a build must not record
/// provenance no matter what it had to write. A file this path creates — on a
/// clone where it was never committed, or from a template that gained a file after
/// the project was made — therefore stays unrecorded, and a later `dev new` that
/// wants to replace it refuses and says so rather than assuming it is clean.
pub(crate) fn materialize_recipe_directory(
    recipe_path: &Path,
    recipe: &Recipe,
) -> anyhow::Result<()> {
    materialize_recipe_directory_in(&DevHome::current(), recipe_path, recipe)
}

pub(crate) fn materialize_recipe_directory_in(
    dev_home: &DevHome,
    recipe_path: &Path,
    recipe: &Recipe,
) -> anyhow::Result<()> {
    let recipe_dir = recipe_dir_of(recipe_path)?;
    prepare_recipe_directory_in(dev_home, recipe, recipe_dir, AuxPolicy::FillMissing)?;
    Ok(())
}

struct ComposeDetails {
    value: Value,
    base_feature_ids: HashSet<String>,
}

fn compose_config_details_in(
    dev_home: &DevHome,
    recipe: &Recipe,
    runtime_name: &str,
    include_base: bool,
) -> anyhow::Result<ComposeDetails> {
    // Layer 1: Global template
    let global_config_path = dev_home.global_template_config(&recipe.global_template);
    let mut global = read_json_file(&global_config_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Global template '{}' not found at {}.\n\
             A recipe references its template by name, so a template of that name has to exist \
             on this machine. Run `dev new`, pick the same template, and choose the same scope to \
             recreate it — or point \"globalTemplate\" in recipe.json at one of your existing \
             templates in {}.",
            recipe.global_template,
            global_config_path.display(),
            dev_home.global_dir().display()
        )
    })?;

    // Apply template option substitutions to the global template
    if !recipe.options.is_empty() {
        substitute_template_options(&mut global, &recipe.options);
    }
    let global_feature_ids = declared_feature_ids(&global);
    let mut selected_definition = config_definition(&global);

    // Layer 2: Base config
    let mut base_feature_ids = HashSet::new();
    let base = if include_base {
        let base_config_path = dev_home.base_config();
        let mut base = read_json_file(&base_config_path)?;
        if let Some(ref mut b) = base {
            if let Some(base_dir) = base_config_path.parent() {
                absolutize_config_paths(b, base_dir);
            }
            base_feature_ids = declared_feature_ids(b);
            if let Some(definition) = config_definition(b) {
                selected_definition = Some(definition);
            }
        }
        base
    } else {
        None
    };

    // Layer 3: Runtime config
    let runtime_config_path = dev_home.runtime_config(runtime_name);
    let mut runtime = read_json_file(&runtime_config_path)?;
    if let Some(ref mut r) = runtime {
        if let Some(runtime_dir) = runtime_config_path.parent() {
            absolutize_config_paths(r, runtime_dir);
        }
        if let Some(definition) = config_definition(r) {
            selected_definition = Some(definition);
        }
    }

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
        for feature_ref in &recipe.features {
            base_feature_ids.remove(feature_ref);
        }
    }

    // Apply user customizations as the highest-priority layer
    if !is_empty_object(&recipe.customizations) {
        for id in declared_feature_ids(&recipe.customizations) {
            base_feature_ids.remove(&id);
        }
        if let Some(definition) = config_definition(&recipe.customizations) {
            selected_definition = Some(definition);
        }
        merge_layer(&mut composed, &recipe.customizations);
    }

    for id in global_feature_ids {
        base_feature_ids.remove(&id);
    }

    prune_lower_priority_definitions(&mut composed, selected_definition);

    Ok(ComposeDetails {
        value: composed,
        base_feature_ids,
    })
}

fn declared_feature_ids(value: &Value) -> HashSet<String> {
    value
        .get("features")
        .and_then(Value::as_object)
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// What to do with an auxiliary file that is already present in the recipe directory.
pub(crate) enum AuxPolicy<'a> {
    /// Fill in what is missing and leave everything already on disk alone.
    FillMissing,
    /// Regenerate against the current template. A file matching what `previous`
    /// recorded generating is replaced; anything else is treated as authored and
    /// aborts the whole operation.
    Refresh { previous: Option<&'a Recipe> },
}

struct AuxPlan<'a> {
    recipe: &'a Recipe,
    /// The recipe directory itself, so a message about a nested file still points
    /// at the one `recipe.json` that governs it.
    recipe_dir: &'a Path,
    /// What an earlier run recorded writing here. Empty means provenance is
    /// unknown, which is not the same as knowing a file is clean.
    recorded: &'a BTreeMap<String, String>,
    refresh: bool,
}

/// Write the template's auxiliary files into `dest_dir`, returning the SHA-256 of
/// everything the template generates so the caller can record it on the recipe.
pub(crate) fn prepare_recipe_directory_in(
    dev_home: &DevHome,
    recipe: &Recipe,
    dest_dir: &Path,
    policy: AuxPolicy<'_>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let no_provenance = BTreeMap::new();
    let (refresh, recorded) = match policy {
        AuxPolicy::FillMissing => (false, &no_provenance),
        AuxPolicy::Refresh { previous } => (
            true,
            previous.map_or(&no_provenance, |prev| &prev.generated),
        ),
    };
    let plan = AuxPlan {
        recipe,
        recipe_dir: dest_dir,
        recorded,
        refresh,
    };

    let src_dir = dev_home.global_template_dir(&recipe.global_template);
    let mut planned = Vec::new();
    let mut manifest = BTreeMap::new();
    plan_auxiliary_files(
        &src_dir,
        dest_dir,
        Path::new(""),
        &plan,
        &mut planned,
        &mut manifest,
    )?;

    fs::create_dir_all(dest_dir)?;
    for file in planned {
        if let Some(parent) = file.dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file.dest, &file.content)?;
    }
    Ok(manifest)
}

struct PlannedFile {
    dest: PathBuf,
    content: Vec<u8>,
}

/// Decide what every non-config file in a global template's `.devcontainer/`
/// should become in the destination directory, applying `${templateOption:...}`
/// substitutions. `devcontainer.json` and `devcontainer-template.json` are skipped
/// because the composition pipeline owns them.
///
/// Planning is separated from writing so that a rejected file aborts before any
/// file — or the recipe itself — has been touched.
fn plan_auxiliary_files(
    src_dir: &Path,
    dest_dir: &Path,
    rel_dir: &Path,
    plan: &AuxPlan<'_>,
    planned: &mut Vec<PlannedFile>,
    manifest: &mut BTreeMap<String, String>,
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
        let rel_path = rel_dir.join(&name);
        if src_path.is_dir() {
            plan_auxiliary_files(&src_path, &dest_path, &rel_path, plan, planned, manifest)?;
            continue;
        }

        let source = fs::read(&src_path)?;
        let content = substitute_bytes(&source, &plan.recipe.options);
        let key = manifest_key(&rel_path);
        let generated = hash_bytes(&content);
        manifest.insert(key.clone(), generated.clone());

        let Ok(existing) = fs::read(&dest_path) else {
            planned.push(PlannedFile {
                dest: dest_path,
                content,
            });
            continue;
        };
        if existing == content || !plan.refresh {
            continue;
        }
        if !may_replace_existing(
            &existing,
            &generated,
            plan.recorded.get(&key).map(String::as_str),
            &dest_path,
            plan,
        )? {
            continue;
        }
        planned.push(PlannedFile {
            dest: dest_path,
            content,
        });
    }
    Ok(())
}

/// Decide whether newly generated content may replace what is already on disk.
///
/// `false` means this file's generated output did not change, so there is nothing
/// to refresh and whatever is there stays. The two refusals are kept apart because
/// they send the reader to different places: an unrecorded file may be perfectly
/// clean and simply unaccounted for, while a recorded mismatch really is an edit.
fn may_replace_existing(
    existing: &[u8],
    generated: &str,
    recorded: Option<&str>,
    dest_path: &Path,
    plan: &AuxPlan<'_>,
) -> anyhow::Result<bool> {
    if recorded == Some(generated) {
        return Ok(false);
    }
    let Some(recorded) = recorded else {
        anyhow::bail!(
            "{} already exists and this recipe has no record of generating it, so it cannot be \
             replaced safely. Nothing was changed.\n\
             Recipes created before provenance was tracked, and files first written by `dev up` \
             or `dev build`, carry no record — so this happens even when the file was never \
             edited.\n\
             Check the file, then move or delete it and re-run to take template '{}'s version.",
            dest_path.display(),
            plan.recipe.global_template
        );
    };
    if recorded != hash_bytes(existing) {
        anyhow::bail!(
            "{} has changed since template '{}' generated it, so it was left alone and nothing \
             was changed.\n\
             Move or delete it and re-run to take the template's version, or keep it and edit \
             \"options\" in {} by hand.",
            dest_path.display(),
            plan.recipe.global_template,
            plan.recipe_dir.join("recipe.json").display()
        );
    }
    Ok(true)
}

/// A stable, slash-separated manifest key, so a recipe written on one platform
/// still matches its files on another.
fn manifest_key(rel_path: &Path) -> String {
    rel_path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Apply template option substitutions to a template file, leaving non-UTF-8
/// files (images, archives) byte-identical.
fn substitute_bytes(source: &[u8], options: &HashMap<String, String>) -> Vec<u8> {
    match std::str::from_utf8(source) {
        Ok(text) => crate::devcontainer::templates::substitute_options(text, options).into_bytes(),
        Err(_) => source.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::config::DevcontainerConfig;
    use crate::devcontainer::features::feature_image_tag;
    use crate::devcontainer::resolve_features;
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
                customizations: serde_json::json!({}),
                generated: BTreeMap::new(),
            }
        }

        fn compose(&self, recipe: &Recipe, runtime_name: &str, include_base: bool) -> Value {
            compose_config_in(&self.dev_home, recipe, runtime_name, include_base).unwrap()
        }

        fn recipe_dir(&self) -> PathBuf {
            self.workspace.join(".devcontainer")
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
        assert!(
            composed["features"]
                .as_object()
                .unwrap()
                .contains_key("ghcr.io/features/node")
        );
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
    fn recipe_config_composes_in_memory_without_writing_devcontainer_json() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode", "features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        recipe.write_to(&recipe_path).unwrap();

        let returned =
            compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true).unwrap();

        assert_eq!(returned.value["remoteUser"], "vscode");
        assert!(
            returned
                .config_path
                .ends_with("demo/.devcontainer/devcontainer.json")
        );
        assert!(
            !returned.config_path.exists(),
            "the composed devcontainer.json is a virtual path for recipe projects"
        );
    }

    #[test]
    fn composing_without_base_is_invocation_local() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode", "features": {"ghcr.io/features/zsh": {}}}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        recipe.write_to(&recipe_path).unwrap();

        let with_base =
            compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true)
                .unwrap()
                .value;
        let without_base =
            compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", false)
                .unwrap()
                .value;

        assert_eq!(
            without_base["remoteUser"], "root",
            "the run drops the base layer"
        );
        assert_eq!(with_base["remoteUser"], "vscode");
        assert!(!recipe_dir.join("devcontainer.json").exists());
    }

    #[test]
    fn prepare_recipe_directory_copies_auxiliary_template_files() {
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

        let recipe_dir = env.recipe_dir();
        prepare_recipe_directory_in(
            &env.dev_home,
            &recipe,
            &recipe_dir,
            AuxPolicy::Refresh { previous: None },
        )
        .unwrap();

        let copied = recipe_dir.join("Dockerfile");
        assert_eq!(fs::read_to_string(copied).unwrap(), "FROM rust:latest\n");
        assert!(
            !recipe_dir.join("devcontainer.json").exists(),
            "preparing recipe files must not write composed config state"
        );
    }

    #[test]
    fn materializing_a_recipe_directory_leaves_existing_auxiliary_files_alone() {
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
        fs::write(&aux, "FROM rust:latest\n").unwrap();
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let copied = recipe_dir.join("Dockerfile");
        fs::write(&copied, "FROM project-edited\n").unwrap();

        materialize_recipe_directory_in(&env.dev_home, &recipe_dir.join("recipe.json"), &recipe)
            .unwrap();

        assert_eq!(fs::read_to_string(copied).unwrap(), "FROM project-edited\n");
    }

    /// Run the real `dev new` sequence: generate the auxiliary files, then record
    /// on the recipe what was written, exactly as `write_recipe_in` does.
    fn generate(
        env: &TestDevHome,
        recipe: &mut Recipe,
        previous: Option<&Recipe>,
    ) -> anyhow::Result<()> {
        recipe.generated = prepare_recipe_directory_in(
            &env.dev_home,
            recipe,
            &env.recipe_dir(),
            AuxPolicy::Refresh { previous },
        )?;
        Ok(())
    }

    fn write_template_aux(env: &TestDevHome, template: &str, name: &str, content: &str) {
        let dir = env.dev_home.global_template_dir(template);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    fn add_template(env: &TestDevHome, template: &str, config: &str) {
        let dir = env.dev_home.global_template_dir(template);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("devcontainer.json"), config).unwrap();
    }

    fn dockerfile_env() -> TestDevHome {
        TestDevHome::new(
            r#"{"build": {"dockerfile": "Dockerfile"}}"#,
            None,
            None,
            "docker",
        )
    }

    #[test]
    fn a_recipe_records_the_files_it_generated() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:latest\n");
        let nested = env
            .dev_home
            .global_template_dir("test-lang")
            .join("scripts");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("setup.sh"), "echo hi\n").unwrap();

        let mut recipe = env.recipe();
        generate(&env, &mut recipe, None).unwrap();

        assert_eq!(
            recipe.generated.get("Dockerfile"),
            Some(&hash_bytes(b"FROM rust:latest\n"))
        );
        assert_eq!(
            recipe.generated.get("scripts/setup.sh"),
            Some(&hash_bytes(b"echo hi\n")),
            "nested files are keyed by a slash-separated relative path"
        );
    }

    #[test]
    fn changing_options_regenerates_an_untouched_auxiliary_file() {
        let env = dockerfile_env();
        write_template_aux(
            &env,
            "test-lang",
            "Dockerfile",
            "FROM ${templateOption:base}\n",
        );

        let mut first = env.recipe();
        first.options.insert("base".into(), "rust:1.75".into());
        generate(&env, &mut first, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), "FROM rust:1.75\n");

        let mut second = env.recipe();
        second.options.insert("base".into(), "rust:1.80".into());
        generate(&env, &mut second, Some(&first)).unwrap();

        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:1.80\n",
            "a file still matching what was recorded is generated, so it follows the options"
        );
    }

    #[test]
    fn changing_options_refuses_to_clobber_a_locally_edited_auxiliary_file() {
        let env = dockerfile_env();
        write_template_aux(
            &env,
            "test-lang",
            "Dockerfile",
            "FROM ${templateOption:base}\n",
        );

        let mut first = env.recipe();
        first.options.insert("base".into(), "rust:1.75".into());
        generate(&env, &mut first, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        fs::write(&dockerfile, "FROM rust:1.75\nRUN cargo install just\n").unwrap();

        let mut second = env.recipe();
        second.options.insert("base".into(), "rust:1.80".into());
        let err = generate(&env, &mut second, Some(&first)).unwrap_err();

        assert!(
            err.to_string().contains("Dockerfile"),
            "the refusal should name the file: {err}"
        );
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:1.75\nRUN cargo install just\n",
            "a rejected refresh must leave the project untouched"
        );
    }

    /// Re-running `dev new` only to add a feature changes nothing that generates
    /// auxiliary files, so a customized Dockerfile must survive rather than
    /// blocking the whole command.
    #[test]
    fn rerunning_with_unchanged_options_keeps_a_customized_auxiliary_file() {
        let env = dockerfile_env();
        write_template_aux(
            &env,
            "test-lang",
            "Dockerfile",
            "FROM ${templateOption:base}\n",
        );

        let mut first = env.recipe();
        first.options.insert("base".into(), "rust:1.75".into());
        generate(&env, &mut first, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        fs::write(&dockerfile, "FROM rust:1.75\nRUN cargo install just\n").unwrap();

        let mut second = env.recipe();
        second.options.insert("base".into(), "rust:1.75".into());
        second.features = vec!["ghcr.io/features/node:1".to_string()];
        generate(&env, &mut second, Some(&first)).unwrap();

        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:1.75\nRUN cargo install just\n",
            "an unrelated rerun must not touch or reject a customized file"
        );
    }

    /// Editing a global template is a first-class flow, and the recorded hash is
    /// what makes the resulting change detectable at all.
    #[test]
    fn editing_the_global_template_refreshes_an_untouched_auxiliary_file() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.75\n");

        let mut first = env.recipe();
        generate(&env, &mut first, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), "FROM rust:1.75\n");

        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.80\n");
        let mut second = env.recipe();
        generate(&env, &mut second, Some(&first)).unwrap();

        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:1.80\n",
            "the same template and options can still generate different bytes"
        );
    }

    #[test]
    fn editing_the_global_template_still_refuses_an_authored_file() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.75\n");

        let mut first = env.recipe();
        generate(&env, &mut first, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        fs::write(&dockerfile, "FROM rust:1.75\nRUN cargo install just\n").unwrap();

        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.80\n");
        let mut second = env.recipe();
        let err = generate(&env, &mut second, Some(&first)).unwrap_err();

        assert!(err.to_string().contains("Dockerfile"), "{err}");
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:1.75\nRUN cargo install just\n"
        );
    }

    /// A recipe written before provenance was recorded cannot vouch for anything
    /// on disk, so the safe answer is to stop rather than guess — and to say that
    /// is why, since the file may well be untouched.
    #[test]
    fn a_legacy_recipe_without_provenance_refuses_and_says_nothing_was_recorded() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:latest\n");
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let dockerfile = recipe_dir.join("Dockerfile");
        fs::write(&dockerfile, "FROM handwritten\n").unwrap();

        let legacy = env.recipe();
        assert!(legacy.generated.is_empty());
        let mut next = env.recipe();
        let err = generate(&env, &mut next, Some(&legacy))
            .unwrap_err()
            .to_string();

        assert!(err.contains("Dockerfile"), "{err}");
        assert!(
            err.contains("no record of generating it"),
            "an unrecorded file must not be described as edited: {err}"
        );
        assert!(
            !err.contains("has changed since"),
            "no comparison was possible, so it must not claim one failed: {err}"
        );
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM handwritten\n"
        );
    }

    /// The other refusal really is an edit, and must read like one.
    #[test]
    fn a_recorded_file_that_diverged_reports_a_mismatch_not_missing_provenance() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.75\n");
        let mut first = env.recipe();
        generate(&env, &mut first, None).unwrap();
        assert!(!first.generated.is_empty());
        fs::write(
            env.recipe_dir().join("Dockerfile"),
            "FROM rust:1.75\nRUN cargo install just\n",
        )
        .unwrap();

        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.80\n");
        let mut second = env.recipe();
        let err = generate(&env, &mut second, Some(&first))
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("has changed since"),
            "a recorded file that diverged is an edit: {err}"
        );
        assert!(
            !err.contains("no record of generating it"),
            "provenance existed, so it must not claim otherwise: {err}"
        );
    }

    /// `dev up` writes build context without recording it, because `recipe.json`
    /// is project state that only `dev new` may rewrite.
    #[test]
    fn materializing_for_a_build_records_no_provenance() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:latest\n");
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        let recipe = env.recipe();
        recipe.write_to(&recipe_path).unwrap();
        let recipe_json_before = fs::read_to_string(&recipe_path).unwrap();

        materialize_recipe_directory_in(&env.dev_home, &recipe_path, &recipe).unwrap();

        assert_eq!(
            fs::read_to_string(recipe_dir.join("Dockerfile")).unwrap(),
            "FROM rust:latest\n",
            "the build context still gets written"
        );
        assert_eq!(
            fs::read_to_string(&recipe_path).unwrap(),
            recipe_json_before,
            "a build must not rewrite the recipe"
        );
        assert!(
            Recipe::from_path(&recipe_path)
                .unwrap()
                .generated
                .is_empty()
        );
    }

    /// The deliberate consequence of the above: a file `dev up` created is
    /// unaccounted for, and the refusal must name that rather than blame the user.
    #[test]
    fn a_file_first_written_by_a_build_is_refused_as_unrecorded() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.75\n");
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        let recipe = env.recipe();
        recipe.write_to(&recipe_path).unwrap();
        materialize_recipe_directory_in(&env.dev_home, &recipe_path, &recipe).unwrap();

        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.80\n");
        let mut next = env.recipe();
        let err = generate(&env, &mut next, Some(&recipe))
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("no record of generating it") && err.contains("dev up"),
            "the refusal should name the tool that wrote it: {err}"
        );
        assert_eq!(
            fs::read_to_string(recipe_dir.join("Dockerfile")).unwrap(),
            "FROM rust:1.75\n",
            "a rejected refresh must leave the project untouched"
        );
    }

    #[test]
    fn a_refusal_about_a_nested_file_points_at_the_recipe_root() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:1.75\n");
        let nested = env
            .dev_home
            .global_template_dir("test-lang")
            .join("scripts");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("setup.sh"), "echo one\n").unwrap();

        let mut first = env.recipe();
        generate(&env, &mut first, None).unwrap();
        let script = env.recipe_dir().join("scripts/setup.sh");
        fs::write(&script, "echo edited\n").unwrap();
        fs::write(nested.join("setup.sh"), "echo two\n").unwrap();

        let mut second = env.recipe();
        let err = generate(&env, &mut second, Some(&first))
            .unwrap_err()
            .to_string();

        let recipe_json = env.recipe_dir().join("recipe.json");
        assert!(
            err.contains(recipe_json.to_str().unwrap()),
            "the message should point at the governing recipe.json: {err}"
        );
    }

    #[test]
    fn switching_templates_regenerates_a_file_the_old_template_generated() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:latest\n");
        add_template(&env, "other-lang", r#"{"image": "x"}"#);
        write_template_aux(&env, "other-lang", "Dockerfile", "FROM python:latest\n");

        let mut previous = env.recipe();
        previous.global_template = "other-lang".to_string();
        generate(&env, &mut previous, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM python:latest\n"
        );

        let mut switched = env.recipe();
        generate(&env, &mut switched, Some(&previous)).unwrap();

        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM rust:latest\n",
            "a file matching the previous template's record is generated, not authored"
        );
    }

    #[test]
    fn switching_templates_still_refuses_an_authored_file() {
        let env = dockerfile_env();
        write_template_aux(&env, "test-lang", "Dockerfile", "FROM rust:latest\n");
        add_template(&env, "other-lang", r#"{"image": "x"}"#);
        write_template_aux(&env, "other-lang", "Dockerfile", "FROM python:latest\n");

        let mut previous = env.recipe();
        previous.global_template = "other-lang".to_string();
        generate(&env, &mut previous, None).unwrap();
        let dockerfile = env.recipe_dir().join("Dockerfile");
        fs::write(&dockerfile, "FROM python:latest\nRUN pip install uv\n").unwrap();

        let mut switched = env.recipe();
        let err = generate(&env, &mut switched, Some(&previous)).unwrap_err();

        assert!(err.to_string().contains("Dockerfile"), "{err}");
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            "FROM python:latest\nRUN pip install uv\n"
        );
    }

    #[test]
    fn composing_a_recipe_writes_nothing_to_the_project() {
        let env = TestDevHome::new(
            r#"{"build": {"dockerfile": "Dockerfile"}}"#,
            None,
            None,
            "docker",
        );
        fs::write(
            env.dev_home
                .global_template_dir("test-lang")
                .join("Dockerfile"),
            "FROM rust:latest\n",
        )
        .unwrap();
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        recipe.write_to(&recipe_path).unwrap();

        compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true).unwrap();

        let entries: Vec<_> = fs::read_dir(&recipe_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("recipe.json")],
            "read-only composition must not materialize template files"
        );
    }

    #[test]
    fn base_scalar_changes_propagate_without_regenerating_recipe_state() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        recipe.write_to(&recipe_path).unwrap();
        let recipe_before = fs::read_to_string(&recipe_path).unwrap();

        let first = compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true)
            .unwrap()
            .value;
        fs::write(env.dev_home.base_config(), r#"{"remoteUser": "developer"}"#).unwrap();
        let second = compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true)
            .unwrap()
            .value;

        assert_eq!(first["remoteUser"], "vscode");
        assert_eq!(second["remoteUser"], "developer");
        assert_eq!(fs::read_to_string(&recipe_path).unwrap(), recipe_before);
        assert!(!recipe_dir.join("devcontainer.json").exists());
    }

    #[test]
    fn recipe_feature_image_tag_changes_when_base_scalar_changes() {
        let env = TestDevHome::new(
            r#"{
                "image": "rust:latest",
                "features": {"ghcr.io/features/node": {}}
            }"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();

        let first_value = env.compose(&recipe, "docker", true);
        fs::write(env.dev_home.base_config(), r#"{"remoteUser": "developer"}"#).unwrap();
        let second_value = env.compose(&recipe, "docker", true);

        let tag = |value: Value| {
            let config: DevcontainerConfig = serde_json::from_value(value).unwrap();
            feature_image_tag("vsc-demo", &config, &resolve_features(&config).unwrap())
        };

        assert_ne!(tag(first_value), tag(second_value));
    }

    #[test]
    fn recipe_customizations_preserve_project_over_base_precedence() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "base", "containerEnv": {"SHARED": "base"}}"#),
            None,
            "docker",
        );
        let mut recipe = env.recipe();
        recipe.customizations = serde_json::json!({
            "remoteUser": "project",
            "containerEnv": {"SHARED": "project"}
        });

        let composed = env.compose(&recipe, "docker", true);

        assert_eq!(composed["remoteUser"], "project");
        assert_eq!(composed["containerEnv"]["SHARED"], "project");
    }

    #[test]
    fn recipe_no_base_is_invocation_local_and_skips_base_features() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest"}"#,
            Some(
                r#"{
                    "remoteUser": "vscode",
                    "features": {"ghcr.io/features/zsh": {}}
                }"#,
            ),
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("recipe.json");
        recipe.write_to(&recipe_path).unwrap();

        let without_base =
            compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", false)
                .unwrap();
        let with_base =
            compose_recipe_config_in(&env.dev_home, &recipe_path, &recipe, "docker", true).unwrap();

        assert!(without_base.value.get("remoteUser").is_none());
        assert!(without_base.value.get("features").is_none());
        assert!(without_base.base_feature_ids.is_empty());
        assert_eq!(with_base.value["remoteUser"], "vscode");
        assert!(with_base.base_feature_ids.contains("ghcr.io/features/zsh"));
    }

    #[test]
    fn recipe_selector_conflicts_prune_lower_priority_definitions() {
        let env = TestDevHome::new(
            r#"{"build": {"dockerfile": "Dockerfile"}}"#,
            Some(r#"{"image": "base:latest"}"#),
            None,
            "docker",
        );

        let composed = env.compose(&env.recipe(), "docker", true);

        assert_eq!(composed["image"], "base:latest");
        assert!(
            composed.get("build").is_none(),
            "a higher-priority image selector must remove the lower-priority build selector"
        );
    }

    #[test]
    fn recipe_customization_selector_wins_over_base_selector() {
        let env = TestDevHome::new(
            r#"{"build": {"dockerfile": "Dockerfile"}}"#,
            Some(r#"{"image": "base:latest"}"#),
            None,
            "docker",
        );
        let mut recipe = env.recipe();
        recipe.customizations = serde_json::json!({
            "build": {"dockerfile": "Project.Dockerfile"}
        });

        let composed = env.compose(&recipe, "docker", true);

        assert_eq!(composed["build"]["dockerfile"], "Project.Dockerfile");
        assert!(composed.get("image").is_none());
    }

    #[test]
    fn runtime_relative_paths_keep_runtime_provenance_in_recipe_composition() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest"}"#,
            None,
            Some(r#"{"build": {"dockerfile": "Runtime.Dockerfile", "context": "."}}"#),
            "docker",
        );
        let runtime_dir = env
            .dev_home
            .runtime_config("docker")
            .parent()
            .unwrap()
            .to_path_buf();

        let composed = env.compose(&env.recipe(), "docker", true);

        assert_eq!(
            composed["build"]["dockerfile"],
            runtime_dir
                .join("Runtime.Dockerfile")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            composed["build"]["context"],
            runtime_dir.join(".").to_string_lossy().as_ref()
        );
        assert!(composed.get("image").is_none());
    }

    /// `dev shell` and `dev exec` resolve the session user and working directory
    /// through this. A recipe project has no `devcontainer.json` to read, so if
    /// this stops composing they silently fall back to the image's defaults —
    /// a root shell in the wrong directory.
    #[test]
    fn a_recipe_project_resolves_its_remote_user_and_workspace_folder() {
        let env = TestDevHome::new(
            r#"{
                "image": "rust:latest",
                "remoteUser": "vscode",
                "workspaceFolder": "/srv/app"
            }"#,
            None,
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        recipe.write_to(&recipe_dir.join("recipe.json")).unwrap();

        let (config_path, config) =
            load_workspace_config_in(&env.dev_home, &env.workspace, "docker").unwrap();

        assert_eq!(config.remote_user.as_deref(), Some("vscode"));
        assert_eq!(config.workspace_folder.as_deref(), Some("/srv/app"));
        assert!(config_path.ends_with("demo/.devcontainer/devcontainer.json"));
    }

    #[test]
    fn a_recipe_project_picks_up_the_base_layer_the_container_was_built_with() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "root"}"#,
            Some(r#"{"remoteUser": "vscode"}"#),
            None,
            "docker",
        );
        let recipe = env.recipe();
        let recipe_dir = env.recipe_dir();
        fs::create_dir_all(&recipe_dir).unwrap();
        recipe.write_to(&recipe_dir.join("recipe.json")).unwrap();

        let (_, config) =
            load_workspace_config_in(&env.dev_home, &env.workspace, "docker").unwrap();

        assert_eq!(config.remote_user.as_deref(), Some("vscode"));
    }

    /// The user-scope fallback must resolve inside the injected home, or this
    /// test would read the developer's real `~/.dev/devcontainers`.
    #[test]
    fn a_user_scoped_recipe_resolves_within_the_injected_dev_home() {
        let env = TestDevHome::new(
            r#"{"image": "rust:latest", "remoteUser": "vscode"}"#,
            None,
            None,
            "docker",
        );
        let folder_name = crate::util::workspace_folder_name(&env.workspace);
        let user_dir = env
            .dev_home
            .devcontainers_dir()
            .join(&folder_name)
            .join(".devcontainer");
        fs::create_dir_all(&user_dir).unwrap();
        env.recipe()
            .write_to(&user_dir.join("recipe.json"))
            .unwrap();

        let (config_path, config) =
            load_workspace_config_in(&env.dev_home, &env.workspace, "docker").unwrap();

        assert_eq!(config.remote_user.as_deref(), Some("vscode"));
        assert!(
            config_path.starts_with(env.dev_home.devcontainers_dir()),
            "the user-scope config must come from the injected home: {}",
            config_path.display()
        );
    }

    #[test]
    fn a_direct_project_still_loads_its_own_devcontainer_json() {
        let env = TestDevHome::new(r#"{"image": "unused"}"#, None, None, "docker");
        let devcontainer_dir = env.workspace.join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"image": "ubuntu:24.04", "remoteUser": "dev"}"#,
        )
        .unwrap();

        let (config_path, config) =
            load_workspace_config_in(&env.dev_home, &env.workspace, "docker").unwrap();

        assert_eq!(config.remote_user.as_deref(), Some("dev"));
        assert_eq!(config_path, devcontainer_dir.join("devcontainer.json"));
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
        assert!(
            message.contains("dev new"),
            "should tell the user how to recreate it: {message}"
        );
    }
}
