use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::DevError;
use crate::oci::{download_artifact, extract_archive, sha256_hex};

use super::config::{DevcontainerConfig, LifecycleCommand};
use super::jsonc::parse_jsonc;

/// Metadata from `devcontainer-feature.json` inside a feature artifact.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FeatureJsonMeta {
    #[serde(default)]
    install_after: Option<Vec<String>>,
    #[serde(default)]
    depends_on: Option<HashMap<String, serde_json::Value>>,
    #[serde(default)]
    container_env: Option<HashMap<String, String>>,
    #[serde(default)]
    mounts: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    init: Option<bool>,
    #[serde(default)]
    privileged: Option<bool>,
    #[serde(default)]
    cap_add: Option<Vec<String>>,
    #[serde(default)]
    security_opt: Option<Vec<String>>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    on_create_command: Option<serde_json::Value>,
    #[serde(default)]
    post_create_command: Option<serde_json::Value>,
    #[serde(default)]
    post_start_command: Option<serde_json::Value>,
    #[serde(default)]
    post_attach_command: Option<serde_json::Value>,
}

/// A resolved devcontainer feature ready for installation.
#[derive(Debug, Clone)]
pub struct ResolvedFeature {
    pub id: String,
    pub oci_ref: String,
    pub version: String,
    pub options: serde_json::Value,
    pub install_script_path: PathBuf,
    /// Features that must be installed before this one (soft ordering hint).
    pub install_after: Vec<String>,
    /// Environment variables to set in the container from this feature.
    pub container_env: HashMap<String, String>,
    /// Mount specifications from this feature.
    pub mounts: Vec<serde_json::Value>,
    /// Whether this feature requires an init process.
    pub init: bool,
    /// Whether this feature requires privileged mode.
    pub privileged: bool,
    /// Additional Linux capabilities required by this feature.
    pub cap_add: Vec<String>,
    /// Security options required by this feature.
    pub security_opt: Vec<String>,
    /// Custom entrypoint from this feature.
    pub entrypoint: Option<String>,
    /// Lifecycle hooks contributed by this feature.
    pub lifecycle_hooks: FeatureLifecycleHooks,
    /// Whether this feature was added as a transitive dependency via `dependsOn`.
    pub is_dependency: bool,
}

/// Lifecycle hooks declared by a feature in its `devcontainer-feature.json`.
#[derive(Debug, Clone, Default)]
pub struct FeatureLifecycleHooks {
    pub on_create_command: Option<LifecycleCommand>,
    pub post_create_command: Option<LifecycleCommand>,
    pub post_start_command: Option<LifecycleCommand>,
    pub post_attach_command: Option<LifecycleCommand>,
}

/// Parse a lifecycle command from a JSON value (string, array, or object).
fn parse_lifecycle_command(val: &serde_json::Value) -> Option<LifecycleCommand> {
    match val {
        serde_json::Value::String(s) => Some(LifecycleCommand::Single(s.clone())),
        serde_json::Value::Array(arr) => {
            let strs: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if strs.is_empty() {
                None
            } else {
                Some(LifecycleCommand::Multiple(strs))
            }
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<String, String> = obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            if map.is_empty() {
                None
            } else {
                Some(LifecycleCommand::Parallel(map))
            }
        }
        _ => None,
    }
}

/// Determine the kind of feature reference.
enum FeatureRefKind {
    /// Local path (starts with `./` or `../`).
    Local(PathBuf),
    /// Direct tarball URL (starts with `https://`).
    Tarball(String),
    /// OCI registry reference (everything else).
    Oci { oci_ref: String, version: String },
}

/// Parse feature references from the config and resolve them into installable features.
pub fn resolve_features(config: &DevcontainerConfig) -> Result<Vec<ResolvedFeature>, DevError> {
    let features = match &config.features {
        Some(f) if !f.is_empty() => f,
        _ => return Ok(Vec::new()),
    };

    let mut resolved = Vec::new();
    for (id, options) in features {
        let (oci_ref, version) = match classify_feature_ref(id) {
            FeatureRefKind::Local(_) | FeatureRefKind::Tarball(_) => {
                // For local/tarball features, oci_ref stores the original id
                // and version is unused. The actual path is resolved during download.
                (id.clone(), String::new())
            }
            FeatureRefKind::Oci { oci_ref, version } => (oci_ref, version),
        };
        resolved.push(ResolvedFeature {
            id: id.clone(),
            oci_ref,
            version,
            options: options.clone(),
            install_script_path: PathBuf::new(),
            install_after: Vec::new(),
            container_env: HashMap::new(),
            mounts: Vec::new(),
            init: false,
            privileged: false,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            entrypoint: None,
            lifecycle_hooks: FeatureLifecycleHooks::default(),
            is_dependency: false,
        });
    }

    Ok(resolved)
}

/// Ids of `roots` plus every feature they transitively require via `dependsOn`.
///
/// Feature provenance is not recorded on `ResolvedFeature`, so the dependency
/// closure has to be recomputed from the downloaded metadata. Callers use this to
/// separate the features a project owns from the ones a lower-priority layer
/// (e.g. `~/.dev/base/devcontainer.json`) contributed.
pub fn features_required_by(
    features: &[ResolvedFeature],
    roots: &HashSet<String>,
) -> HashSet<String> {
    let by_id: HashMap<&str, &ResolvedFeature> =
        features.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut reachable: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = roots.iter().cloned().collect();

    while let Some(id) = queue.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        let Some(feature) = by_id.get(id.as_str()) else {
            continue;
        };
        if let Some(deps) = read_depends_on(feature) {
            for dep_id in deps.keys() {
                if !reachable.contains(dep_id) {
                    queue.push(dep_id.clone());
                }
            }
        }
    }

    reachable
}

/// Tag for the image produced by layering `features` onto the config's base image.
///
/// The digest suffix makes the tag a true cache key, so an image built from a
/// different effective config (for example one that merged
/// `~/.dev/base/devcontainer.json` when this one did not) can never be mistaken
/// for a cache hit.
///
/// The hashed inputs must stay in step with everything
/// [`generate_feature_dockerfile_with_opts`] bakes into the image: the base image
/// selector, the declared features, the `_REMOTE_USER`/`_CONTAINER_USER` build
/// environment derived from `remoteUser`, and the `containerEnv`/`remoteEnv` maps
/// written into the `devcontainer.metadata` label. Fields that never reach the
/// image (`forwardPorts`, `name`, `customizations`, …) are deliberately excluded
/// so unrelated edits do not force a rebuild.
///
/// `features` may be either the declared set or the post-download set including
/// transitive dependencies; dependencies are filtered out so both yield the same
/// digest.
pub fn feature_image_tag(
    folder_image: &str,
    config: &DevcontainerConfig,
    features: &[ResolvedFeature],
) -> String {
    use sha2::{Digest, Sha256};

    let mut declared: Vec<(&str, &serde_json::Value)> = features
        .iter()
        .filter(|f| !f.is_dependency)
        .map(|f| (f.id.as_str(), &f.options))
        .collect();
    declared.sort_by(|a, b| a.0.cmp(b.0));

    let build = config.build.as_ref().map(|b| {
        serde_json::json!({
            "dockerfile": b.dockerfile,
            "context": b.context,
            "args": b.args,
        })
    });
    fn sorted_env(env: &Option<HashMap<String, String>>) -> Option<Vec<(&str, &str)>> {
        env.as_ref().map(|map| {
            let mut pairs: Vec<(&str, &str)> =
                map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            pairs
        })
    }
    let inputs = serde_json::json!({
        "image": config.image,
        "build": build,
        "features": declared,
        "remoteUser": config.remote_user,
        "containerEnv": sorted_env(&config.container_env),
        "remoteEnv": sorted_env(&config.remote_env),
    });

    let mut hasher = Sha256::new();
    hasher.update(inputs.to_string().as_bytes());
    let digest = hex::encode(hasher.finalize());

    format!("{folder_image}-features-{}", &digest[..12])
}

/// Classify a feature reference string into its kind.
fn classify_feature_ref(id: &str) -> FeatureRefKind {
    if id.starts_with("./") || id.starts_with("../") || id.starts_with('/') {
        FeatureRefKind::Local(PathBuf::from(id))
    } else if id.starts_with("https://") {
        FeatureRefKind::Tarball(id.to_string())
    } else {
        let (oci_ref, version) = parse_feature_ref(id);
        FeatureRefKind::Oci { oci_ref, version }
    }
}

/// Download OCI artifacts for each feature, populating `install_script_path` and metadata fields.
/// On download failure, prompts the user to skip the feature or abort.
///
/// After downloading all user-specified features, this also resolves transitive
/// `dependsOn` dependencies recursively.
pub async fn download_features(
    features: &mut Vec<ResolvedFeature>,
    devcontainer_dir: Option<&std::path::Path>,
) -> Result<(), DevError> {
    // First pass: download all explicitly listed features.
    for feature in features.iter_mut() {
        download_single_feature(feature, devcontainer_dir).await?;
    }

    // Second pass: resolve transitive dependsOn dependencies.
    resolve_depends_on(features, devcontainer_dir).await?;

    Ok(())
}

/// Download a single feature artifact based on its reference kind.
async fn download_single_feature(
    feature: &mut ResolvedFeature,
    devcontainer_dir: Option<&std::path::Path>,
) -> Result<(), DevError> {
    let extracted_dir = match classify_feature_ref(&feature.id) {
        FeatureRefKind::Local(rel_path) => {
            // Resolve relative to .devcontainer/ directory.
            let base = devcontainer_dir.ok_or_else(|| {
                DevError::FeatureNotFound(format!(
                    "Cannot resolve local feature '{}': no .devcontainer directory",
                    feature.id
                ))
            })?;
            let abs_path = base.join(&rel_path);
            if !abs_path.exists() {
                return Err(DevError::FeatureNotFound(format!(
                    "Local feature directory not found: {}",
                    abs_path.display()
                )));
            }
            abs_path
        }
        FeatureRefKind::Tarball(url) => download_tarball_feature(&url).await?,
        FeatureRefKind::Oci { .. } => {
            let result = download_artifact(&feature.oci_ref, &feature.version).await;
            match result {
                Ok(dir) => dir,
                Err(e) => {
                    eprintln!("Warning: failed to download feature '{}': {e}", feature.id);
                    if feature.is_dependency {
                        // Dependencies are mandatory — don't prompt, just fail.
                        return Err(DevError::Registry(format!(
                            "required dependency '{}': {e}",
                            feature.id
                        )));
                    }
                    let skip = dialoguer::Confirm::new()
                        .with_prompt(format!("Skip feature '{}' and continue?", feature.id))
                        .default(true)
                        .interact()
                        .unwrap_or(false);
                    if skip {
                        return Ok(());
                    }
                    return Err(DevError::Registry(format!("feature '{}': {e}", feature.id)));
                }
            }
        }
    };

    // Verify install.sh exists
    let install_sh = extracted_dir.join("install.sh");
    if !install_sh.exists() {
        return Err(DevError::FeatureNotFound(format!(
            "install.sh not found in feature '{}'",
            feature.id
        )));
    }

    // Read optional devcontainer-feature.json for metadata
    let meta_path = extracted_dir.join("devcontainer-feature.json");
    if meta_path.exists() {
        let content = std::fs::read_to_string(&meta_path)?;
        let meta: FeatureJsonMeta = parse_jsonc(&content)?;
        apply_feature_metadata(feature, &meta);
    }

    feature.install_script_path = extracted_dir;
    Ok(())
}

/// Apply parsed metadata from devcontainer-feature.json to a ResolvedFeature.
fn apply_feature_metadata(feature: &mut ResolvedFeature, meta: &FeatureJsonMeta) {
    if let Some(ref install_after) = meta.install_after {
        feature.install_after = install_after.clone();
    }
    if let Some(ref container_env) = meta.container_env {
        feature.container_env = container_env.clone();
    }
    if let Some(ref mounts) = meta.mounts {
        feature.mounts = mounts.clone();
    }
    if let Some(init) = meta.init {
        feature.init = init;
    }
    if let Some(privileged) = meta.privileged {
        feature.privileged = privileged;
    }
    if let Some(ref cap_add) = meta.cap_add {
        feature.cap_add = cap_add.clone();
    }
    if let Some(ref security_opt) = meta.security_opt {
        feature.security_opt = security_opt.clone();
    }
    if meta.entrypoint.is_some() {
        feature.entrypoint = meta.entrypoint.clone();
    }

    // Parse lifecycle hooks
    if let Some(ref val) = meta.on_create_command {
        feature.lifecycle_hooks.on_create_command = parse_lifecycle_command(val);
    }
    if let Some(ref val) = meta.post_create_command {
        feature.lifecycle_hooks.post_create_command = parse_lifecycle_command(val);
    }
    if let Some(ref val) = meta.post_start_command {
        feature.lifecycle_hooks.post_start_command = parse_lifecycle_command(val);
    }
    if let Some(ref val) = meta.post_attach_command {
        feature.lifecycle_hooks.post_attach_command = parse_lifecycle_command(val);
    }
}

/// Read `depends_on` from a feature's `devcontainer-feature.json`, if present.
fn read_depends_on(feature: &ResolvedFeature) -> Option<HashMap<String, serde_json::Value>> {
    let meta_path = feature
        .install_script_path
        .join("devcontainer-feature.json");
    if !meta_path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&meta_path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!(
                "Warning: cannot read feature metadata for '{}' at {}: {e}. \
                 Its dependencies will be treated as absent.",
                feature.id,
                meta_path.display()
            );
            return None;
        }
    };
    let meta: FeatureJsonMeta = match parse_jsonc(&content) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!(
                "Warning: cannot parse feature metadata for '{}' at {}: {e}. \
                 Its dependencies will be treated as absent.",
                feature.id,
                meta_path.display()
            );
            return None;
        }
    };
    meta.depends_on
}

/// Recursively resolve `dependsOn` entries, downloading any features not already present.
async fn resolve_depends_on(
    features: &mut Vec<ResolvedFeature>,
    devcontainer_dir: Option<&std::path::Path>,
) -> Result<(), DevError> {
    let mut visited: HashSet<String> = features.iter().map(|f| f.id.clone()).collect();
    let mut queue: Vec<(String, serde_json::Value)> = Vec::new();

    // Cache depends_on per feature ID to avoid re-reading JSON files.
    let mut deps_cache: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();

    // Collect all dependsOn entries from already-downloaded features.
    for feature in features.iter() {
        if let Some(deps) = read_depends_on(feature) {
            for (dep_id, dep_opts) in &deps {
                if !visited.contains(dep_id) {
                    queue.push((dep_id.clone(), dep_opts.clone()));
                    visited.insert(dep_id.clone());
                }
            }
            deps_cache.insert(feature.id.clone(), deps);
        }
    }

    // Process the queue: download each dependency, read its metadata, and enqueue
    // any of its own dependsOn entries that haven't been visited yet.
    while let Some((dep_id, dep_opts)) = queue.pop() {
        let (oci_ref, version) = match classify_feature_ref(&dep_id) {
            FeatureRefKind::Local(_) | FeatureRefKind::Tarball(_) => {
                (dep_id.clone(), String::new())
            }
            FeatureRefKind::Oci { oci_ref, version } => (oci_ref, version),
        };

        let mut dep_feature = ResolvedFeature {
            id: dep_id.clone(),
            oci_ref,
            version,
            options: dep_opts,
            install_script_path: PathBuf::new(),
            install_after: Vec::new(),
            container_env: HashMap::new(),
            mounts: Vec::new(),
            init: false,
            privileged: false,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            entrypoint: None,
            lifecycle_hooks: FeatureLifecycleHooks::default(),
            is_dependency: true,
        };

        download_single_feature(&mut dep_feature, devcontainer_dir).await?;

        // Check for transitive dependencies in the newly downloaded feature.
        if let Some(deps) = read_depends_on(&dep_feature) {
            for (transitive_id, transitive_opts) in &deps {
                if !visited.contains(transitive_id) {
                    queue.push((transitive_id.clone(), transitive_opts.clone()));
                    visited.insert(transitive_id.clone());
                }
            }
            deps_cache.insert(dep_feature.id.clone(), deps);
        }

        // Add the dependency as an install_after for feature(s) that depend on it,
        // using the cached metadata instead of re-reading files.
        for f in features.iter_mut() {
            if let Some(deps) = deps_cache.get(&f.id)
                && deps.contains_key(&dep_id)
            {
                f.install_after.push(dep_id.clone());
            }
        }

        features.push(dep_feature);
    }

    Ok(())
}

/// Download a feature distributed as a tarball URL.
async fn download_tarball_feature(url: &str) -> Result<PathBuf, DevError> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| DevError::Registry(format!("Failed to download tarball {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(DevError::Registry(format!(
            "HTTP {} downloading tarball {url}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| DevError::Registry(format!("Failed to read tarball {url}: {e}")))?;

    let digest = sha256_hex(&bytes);
    let extract_dir = std::env::temp_dir().join(format!(
        "dev-feature-tarball-{}",
        &digest[..16.min(digest.len())]
    ));

    if !extract_dir.exists() {
        extract_archive(&bytes, &extract_dir)?;
    }

    Ok(extract_dir)
}

/// Stage feature files into a temp directory for use as a Docker build context.
///
/// Each feature's extracted directory is copied into `staging_dir/{i}/` where `i`
/// is the feature's index in the ordered list.
pub fn stage_feature_context(features: &[ResolvedFeature]) -> Result<PathBuf, DevError> {
    let staging_dir = std::env::temp_dir().join(format!("dev-features-{}", std::process::id()));
    std::fs::create_dir_all(&staging_dir)?;

    // We package each feature as a tarball rather than a plain directory
    // because Apple Containers' `container build` has a bug where files
    // inside subdirectories of the build context are not transferred.
    // Using `ADD <tarball>` in the Dockerfile works around this since
    // ADD auto-extracts archives and root-level files transfer correctly.
    for (i, feature) in features.iter().enumerate() {
        if feature.install_script_path.as_os_str().is_empty() {
            continue;
        }
        let tar_path = staging_dir.join(format!("{i}.tar"));
        create_tar(&feature.install_script_path, &tar_path)?;
    }

    Ok(staging_dir)
}

/// Create a tar archive of a directory's contents (without the directory itself).
fn create_tar(src_dir: &std::path::Path, tar_path: &std::path::Path) -> Result<(), DevError> {
    use std::fs::File;
    let file = File::create(tar_path)?;
    let mut builder = tar::Builder::new(file);
    // Append contents of the directory, preserving relative paths.
    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        if path.is_dir() {
            builder
                .append_dir_all(&name, &path)
                .map_err(|e| DevError::Runtime(format!("Failed to tar directory: {e}")))?;
        } else {
            builder
                .append_path_with_name(&path, &name)
                .map_err(|e| DevError::Runtime(format!("Failed to tar file: {e}")))?;
        }
    }
    builder
        .finish()
        .map_err(|e| DevError::Runtime(format!("Failed to finalize tar: {e}")))?;
    Ok(())
}

/// Convert an option name to an environment variable name per the devcontainer spec:
/// replace non-alphanumeric/underscore chars with `_`, strip leading digits/underscores, uppercase.
fn option_name_to_env(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let result = if trimmed.is_empty() {
        &sanitized
    } else {
        trimmed
    };
    result.to_uppercase()
}

/// Parse a feature reference like "ghcr.io/devcontainers/features/node:1" into (ref, version).
fn parse_feature_ref(id: &str) -> (String, String) {
    if let Some((base, version)) = id.rsplit_once(':') {
        (base.to_string(), version.to_string())
    } else {
        (id.to_string(), "latest".to_string())
    }
}

/// Sort features by their `install_after` dependencies (topological sort).
pub fn order_features(features: &[ResolvedFeature]) -> Vec<ResolvedFeature> {
    let id_to_idx: HashMap<&str, usize> = features
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; features.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); features.len()];

    for (i, f) in features.iter().enumerate() {
        for dep_id in &f.install_after {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                dependents[dep_idx].push(i);
                in_degree[i] += 1;
            }
        }
    }

    // Kahn's algorithm
    let mut queue: Vec<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|&(_, d)| *d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut ordered = Vec::with_capacity(features.len());

    while let Some(idx) = queue.pop() {
        ordered.push(features[idx].clone());
        for &dep_idx in &dependents[idx] {
            in_degree[dep_idx] -= 1;
            if in_degree[dep_idx] == 0 {
                queue.push(dep_idx);
            }
        }
    }

    // If there are cycles, append remaining features in original order.
    if ordered.len() < features.len() {
        for (i, f) in features.iter().enumerate() {
            if in_degree[i] > 0 {
                ordered.push(f.clone());
            }
        }
    }

    ordered
}

/// Generate a composite Dockerfile that installs all features on top of a base image.
///
/// Per the devcontainer spec, feature install scripts expect several environment
/// variables to be set by the orchestrating tool.  We inject them right after the
/// FROM line so every RUN step can see them.
///
/// Gap 1 fix: Each feature's `containerEnv` is emitted as `ENV` directives.
/// Gap 3 fix: `_REMOTE_USER_HOME` is resolved dynamically via `getent passwd`.
/// Gap 4 fix: A `LABEL devcontainer.metadata` is appended with merged metadata.
/// Gap 12 fix: Feature install scripts are wrapped with env sourcing and error context.
pub fn generate_feature_dockerfile_with_opts(
    base_image: &str,
    features: &[ResolvedFeature],
    remote_user: Option<&str>,
    config: &DevcontainerConfig,
) -> String {
    let user = remote_user.unwrap_or("root");

    let mut lines: Vec<String> = Vec::new();

    lines.push(format!("FROM {base_image}"));

    // Set _REMOTE_USER / _CONTAINER_USER immediately so later RUN steps can reference them.
    lines.push(format!("ENV _REMOTE_USER=\"{user}\""));
    lines.push(format!("ENV _CONTAINER_USER=\"{user}\""));

    // Static fallback for standard users; overridden dynamically below.
    let static_home = if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    };
    lines.push(format!("ENV _REMOTE_USER_HOME=\"{static_home}\""));
    lines.push(format!("ENV _CONTAINER_USER_HOME=\"{static_home}\""));

    // Dynamically resolve home directory at build time using getent passwd.
    // This handles non-standard users like postgres (/var/lib/postgresql),
    // nginx (/var/cache/nginx), etc. instead of assuming /home/<user>.
    // Write a helper script that feature install scripts will source.
    lines.push(format!(
        "RUN _HOME=$(getent passwd \"{user}\" 2>/dev/null | cut -d: -f6) && \
         if [ -n \"$_HOME\" ]; then \
           echo \"export _REMOTE_USER_HOME=$_HOME\" > /usr/local/share/dev-container-user-home.sh && \
           echo \"export _CONTAINER_USER_HOME=$_HOME\" >> /usr/local/share/dev-container-user-home.sh; \
         fi"
    ));

    for (i, feature) in features.iter().enumerate() {
        let stage_dir = format!("/tmp/dev-features/{i}");
        if feature.install_script_path.as_os_str().is_empty() {
            continue;
        }

        // Emit feature's containerEnv as ENV directives (Gap 1).
        // These intentionally persist in the final image — they are part of
        // the container's runtime environment, not build-time options.
        for (key, val) in &feature.container_env {
            let escaped_val = val.replace('\\', "\\\\").replace('"', "\\\"");
            lines.push(format!("ENV {key}=\"{escaped_val}\""));
        }

        // Collect feature options to pass as scoped exports in the RUN step.
        // Options must NOT be emitted as ENV directives because ENV persists
        // across all subsequent Dockerfile steps. A feature setting e.g.
        // VERSION=3.12 would leak into later features that use $VERSION with
        // a different meaning (e.g. copilot-cli defaulting to "latest").
        let mut option_exports = Vec::new();
        if let Some(obj) = feature.options.as_object() {
            for (key, val) in obj {
                let env_name = option_name_to_env(key);
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                // Escape special characters for safe embedding in a printf '%b'
                // expression inside a Dockerfile RUN step. This handles newlines,
                // tabs, carriage returns, backslashes, and single quotes without
                // breaking the Dockerfile syntax.
                let escaped_val = val_str
                    .replace('\\', "\\\\")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
                    .replace('\'', "'\\''");
                option_exports.push(format!(
                    "export {env_name}=\"$(printf '%b' '{escaped_val}')\""
                ));
            }
        }

        // Always use ADD to extract the feature tarball from the uploaded build
        // context. RUN --mount=type=bind requires a BuildKit gRPC session server
        // that Bollard does not start, causing "context not found" on Linux with
        // older Docker Engine (Docker Desktop on Mac silently works around it).
        lines.push(format!("ADD {i}.tar {stage_dir}/"));

        // Wrapper script with env sourcing, scoped options, and error context.
        lines.push(format!(
            "RUN {wrapper}",
            wrapper =
                feature_wrapper_script(&feature.id, &feature.version, &stage_dir, &option_exports),
        ));
    }

    // Build and emit the devcontainer.metadata label (Gap 4).
    let metadata_label = build_metadata_label(features, config, remote_user);
    // Escape the JSON for use in a Dockerfile LABEL.
    // Dollar signs must be doubled so Docker's builder treats them as literals
    // rather than variable substitutions (e.g. ${localEnv:...} would fail).
    let escaped = metadata_label
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$");
    lines.push(format!("LABEL devcontainer.metadata=\"{escaped}\""));

    lines.join("\n")
}

/// Generate the wrapper script that sources environment files and provides
/// error context around a feature's install.sh (Gap 12).
///
/// The wrapper:
/// 1. Sources the dynamic user home script (for non-standard users)
/// 2. Exports feature options as scoped environment variables
/// 3. Sets error context variables (_DEV_FEATURE_ID, _DEV_FEATURE_VERSION)
/// 4. Runs install.sh with `set -e` for proper error propagation
/// 5. Reports clear error messages on failure
fn feature_wrapper_script(
    feature_id: &str,
    feature_version: &str,
    stage_dir: &str,
    option_exports: &[String],
) -> String {
    // Shell-escape the feature ID for safe embedding in the script.
    let escaped_id = feature_id.replace('\'', "'\\''");
    let escaped_version = feature_version.replace('\'', "'\\''");

    let options_block = if option_exports.is_empty() {
        String::new()
    } else {
        format!("{} && ", option_exports.join(" && "))
    };

    format!(
        "set -e && \
         if [ -f /usr/local/share/dev-container-user-home.sh ]; then \
           . /usr/local/share/dev-container-user-home.sh; \
         fi && \
         {options_block}\
         export _DEV_FEATURE_ID='{escaped_id}' && \
         export _DEV_FEATURE_VERSION='{escaped_version}' && \
         cd {stage_dir} && \
         chmod +x install.sh && \
         if ! ./install.sh; then \
           echo \"ERROR: Feature '{escaped_id}' (version '{escaped_version}') install.sh failed\" >&2; \
           exit 1; \
         fi"
    )
}

/// Build the JSON array for the `devcontainer.metadata` image label.
///
/// Includes one entry per feature (with its contributed containerEnv, mounts,
/// capabilities, lifecycle hooks) followed by one entry for the base devcontainer.json
/// config (remoteUser, containerEnv, lifecycle hooks, etc.).
fn build_metadata_label(
    features: &[ResolvedFeature],
    config: &DevcontainerConfig,
    remote_user: Option<&str>,
) -> String {
    let mut metadata: Vec<serde_json::Value> = Vec::new();

    // Feature entries.
    for feature in features {
        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), serde_json::Value::String(feature.id.clone()));

        if !feature.container_env.is_empty() {
            entry.insert(
                "containerEnv".into(),
                serde_json::to_value(&feature.container_env).unwrap_or_default(),
            );
        }
        if !feature.mounts.is_empty() {
            entry.insert(
                "mounts".into(),
                serde_json::Value::Array(feature.mounts.clone()),
            );
        }
        if feature.init {
            entry.insert("init".into(), serde_json::Value::Bool(true));
        }
        if feature.privileged {
            entry.insert("privileged".into(), serde_json::Value::Bool(true));
        }
        if !feature.cap_add.is_empty() {
            entry.insert(
                "capAdd".into(),
                serde_json::to_value(&feature.cap_add).unwrap_or_default(),
            );
        }
        if !feature.security_opt.is_empty() {
            entry.insert(
                "securityOpt".into(),
                serde_json::to_value(&feature.security_opt).unwrap_or_default(),
            );
        }
        if feature.entrypoint.is_some() {
            entry.insert(
                "entrypoint".into(),
                serde_json::Value::String(feature.entrypoint.clone().unwrap_or_default()),
            );
        }

        // Include lifecycle hooks in metadata so they survive the build.
        insert_lifecycle_hook(
            &mut entry,
            "onCreateCommand",
            &feature.lifecycle_hooks.on_create_command,
        );
        insert_lifecycle_hook(
            &mut entry,
            "postCreateCommand",
            &feature.lifecycle_hooks.post_create_command,
        );
        insert_lifecycle_hook(
            &mut entry,
            "postStartCommand",
            &feature.lifecycle_hooks.post_start_command,
        );
        insert_lifecycle_hook(
            &mut entry,
            "postAttachCommand",
            &feature.lifecycle_hooks.post_attach_command,
        );

        metadata.push(serde_json::Value::Object(entry));
    }

    // Base config entry.
    let mut base_entry = serde_json::Map::new();
    if let Some(user) = remote_user {
        base_entry.insert(
            "remoteUser".into(),
            serde_json::Value::String(user.to_string()),
        );
    }
    if let Some(ref env) = config.container_env {
        base_entry.insert(
            "containerEnv".into(),
            serde_json::to_value(env).unwrap_or_default(),
        );
    }
    if let Some(ref remote_env) = config.remote_env {
        base_entry.insert(
            "remoteEnv".into(),
            serde_json::to_value(remote_env).unwrap_or_default(),
        );
    }
    metadata.push(serde_json::Value::Object(base_entry));

    serde_json::to_string(&metadata).unwrap_or_else(|_| "[]".to_string())
}

/// Insert a lifecycle hook into a metadata entry if it's Some.
fn insert_lifecycle_hook(
    entry: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    hook: &Option<LifecycleCommand>,
) {
    if let Some(cmd) = hook {
        let val = match cmd {
            LifecycleCommand::Single(s) => serde_json::Value::String(s.clone()),
            LifecycleCommand::Multiple(arr) => serde_json::Value::Array(
                arr.iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
            LifecycleCommand::Parallel(map) => serde_json::to_value(map).unwrap_or_default(),
        };
        entry.insert(key.into(), val);
    }
}

/// Merge feature-contributed container capabilities (init, privileged, capAdd, securityOpt)
/// into a single set of values. Booleans are OR'd, arrays are unioned.
pub fn merge_feature_capabilities(features: &[ResolvedFeature]) -> MergedCapabilities {
    let mut result = MergedCapabilities::default();
    for f in features {
        result.init = result.init || f.init;
        result.privileged = result.privileged || f.privileged;
        for cap in &f.cap_add {
            if !result.cap_add.contains(cap) {
                result.cap_add.push(cap.clone());
            }
        }
        for opt in &f.security_opt {
            if !result.security_opt.contains(opt) {
                result.security_opt.push(opt.clone());
            }
        }
    }
    result
}

/// Aggregated container capabilities from all features.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergedCapabilities {
    pub init: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
}

/// Recover feature-contributed capabilities from an image's `devcontainer.metadata`
/// label entries, as written by `build_metadata_label`.
///
/// This is the counterpart to `merge_feature_capabilities` for the cached-image path,
/// where the features that built the image are not re-resolved and so no
/// `ResolvedFeature` list exists. Semantics match that function: booleans are OR'd and
/// arrays unioned. Keys are read defensively — the writer omits falsey capabilities
/// entirely, and the trailing base-config entry carries none of them.
pub fn capabilities_from_metadata(entries: &[serde_json::Value]) -> MergedCapabilities {
    let mut result = MergedCapabilities::default();
    for entry in entries {
        result.init |= entry_flag(entry, "init");
        result.privileged |= entry_flag(entry, "privileged");
        union_string_array(entry.get("capAdd"), &mut result.cap_add);
        union_string_array(entry.get("securityOpt"), &mut result.security_opt);
    }
    result
}

/// Read a boolean metadata key, treating absent or non-boolean values as false.
fn entry_flag(entry: &serde_json::Value, key: &str) -> bool {
    entry
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Append the string members of a JSON array into `target`, preserving first-seen
/// order and skipping duplicates. Non-string members are ignored.
fn union_string_array(value: Option<&serde_json::Value>, target: &mut Vec<String>) {
    let Some(items) = value.and_then(serde_json::Value::as_array) else {
        return;
    };
    for item in items.iter().filter_map(serde_json::Value::as_str) {
        if !target.iter().any(|existing| existing == item) {
            target.push(item.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(id: &str) -> ResolvedFeature {
        ResolvedFeature {
            id: id.to_string(),
            oci_ref: id.to_string(),
            version: "latest".to_string(),
            options: serde_json::Value::Null,
            install_script_path: PathBuf::new(),
            install_after: Vec::new(),
            container_env: HashMap::new(),
            mounts: Vec::new(),
            init: false,
            privileged: false,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            entrypoint: None,
            lifecycle_hooks: FeatureLifecycleHooks::default(),
            is_dependency: false,
        }
    }

    fn empty_config() -> DevcontainerConfig {
        serde_json::from_str("{}").expect("empty devcontainer.json should deserialize")
    }

    /// README.md tells readers to reclaim disk with a `docker image ls --filter
    /// reference=<glob>`. That glob has to actually match the derived images we
    /// build, or the documented command silently reports nothing to reclaim.
    #[test]
    fn readme_reclaim_filter_matches_derived_image_names() {
        let readme = include_str!("../../README.md");
        let glob = readme
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("docker image ls --filter 'reference=")
            })
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("README.md should document a `docker image ls --filter reference=` command");

        // `<folder>` is a placeholder the reader substitutes; do the same here.
        let glob = glob.replace("<folder>", "webapp");
        let folder_image = crate::util::naming::container_name(std::path::Path::new("/tmp/webapp"));
        let image = feature_image_tag(&folder_image, &empty_config(), &[feature("ghcr.io/x/y")]);

        assert!(
            glob_matches(&glob, &image),
            "README.md documents `reference={glob}` but it does not match `{image}`"
        );
    }

    /// A glob with no trailing wildcard must reject extra text after its final
    /// literal — the old generic matcher would have silently accepted it.
    #[test]
    fn non_trailing_wildcard_glob_rejects_suffix() {
        let glob = "vsc-webapp-*-features";
        let folder_image = crate::util::naming::container_name(std::path::Path::new("/tmp/webapp"));
        let image = feature_image_tag(&folder_image, &empty_config(), &[feature("ghcr.io/x/y")]);

        assert!(
            !glob_matches(glob, &image),
            "non-trailing-wildcard glob must reject extra suffix; got rest={image:?}"
        );
    }

    fn glob_matches(glob: &str, image: &str) -> bool {
        let literals: Vec<&str> = glob.split('*').collect();
        let trailing_wildcard = glob.ends_with('*');
        let mut rest = image;
        for (i, literal) in literals.iter().enumerate() {
            let remainder = if i == 0 {
                rest.strip_prefix(literal)
            } else if !trailing_wildcard && i == literals.len() - 1 {
                rest.ends_with(literal).then_some("")
            } else {
                rest.find(literal).map(|at| &rest[at + literal.len()..])
            };
            rest = match remainder {
                Some(r) => r,
                None => return false,
            };
        }
        trailing_wildcard || rest.is_empty()
    }

    fn parse_label(label: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(label).expect("metadata label should be a JSON array")
    }

    /// The capabilities recovered from a built image must equal those the build itself
    /// derived. This is the guard against key-name drift between `build_metadata_label`
    /// and `capabilities_from_metadata` (e.g. `capAdd` vs `cap_add`), which no
    /// hand-written JSON fixture would catch.
    #[test]
    fn capabilities_survive_metadata_label_roundtrip() {
        let mut dind = feature("ghcr.io/devcontainers/features/docker-in-docker:2");
        dind.privileged = true;
        dind.init = true;
        dind.cap_add = vec!["SYS_PTRACE".to_string()];
        dind.security_opt = vec!["seccomp=unconfined".to_string()];
        let features = vec![dind];

        let label = build_metadata_label(&features, &empty_config(), None);
        let recovered = capabilities_from_metadata(&parse_label(&label));

        assert!(
            recovered.privileged,
            "privileged must survive the roundtrip"
        );
        assert_eq!(recovered, merge_feature_capabilities(&features));
    }

    /// A feature declaring no capabilities must not acquire any via the label, and the
    /// trailing base-config entry must be ignored harmlessly.
    #[test]
    fn plain_feature_roundtrips_without_capabilities() {
        let features = vec![feature("ghcr.io/devcontainers/features/node:1")];

        let label = build_metadata_label(&features, &empty_config(), Some("node"));
        let recovered = capabilities_from_metadata(&parse_label(&label));

        assert_eq!(recovered, MergedCapabilities::default());
    }

    #[test]
    fn capabilities_union_across_entries_and_deduplicate() {
        let entries = vec![
            serde_json::json!({"id": "a", "capAdd": ["SYS_PTRACE"]}),
            serde_json::json!({
                "id": "b",
                "privileged": true,
                "capAdd": ["SYS_PTRACE", "NET_ADMIN"],
                "securityOpt": ["seccomp=unconfined"],
            }),
        ];

        let caps = capabilities_from_metadata(&entries);

        assert!(caps.privileged);
        assert_eq!(caps.cap_add, ["SYS_PTRACE", "NET_ADMIN"]);
        assert_eq!(caps.security_opt, ["seccomp=unconfined"]);
    }

    /// An image with no recoverable metadata must yield no capabilities rather than
    /// panicking or inventing them.
    #[test]
    fn capabilities_from_metadata_defaults_when_absent() {
        assert_eq!(
            capabilities_from_metadata(&[]),
            MergedCapabilities::default()
        );

        let no_caps = capabilities_from_metadata(&[serde_json::json!({"id": "a"})]);
        assert_eq!(no_caps, MergedCapabilities::default());
    }

    /// Malformed values must be ignored, not coerced into capabilities.
    #[test]
    fn capabilities_from_metadata_ignores_malformed_values() {
        let entries = vec![serde_json::json!({
            "id": "a",
            "privileged": "yes",
            "capAdd": "SYS_PTRACE",
            "securityOpt": [42, "seccomp=unconfined"],
        })];

        let caps = capabilities_from_metadata(&entries);

        assert!(!caps.privileged, "a non-boolean must not enable privileged");
        assert!(
            caps.cap_add.is_empty(),
            "a non-array capAdd must be ignored"
        );
        assert_eq!(caps.security_opt, ["seccomp=unconfined"]);
    }

    /// Adapter over `feature` for tests that exercise Dockerfile generation.
    ///
    /// Sets `install_script_path`, without which `generate_feature_dockerfile_with_opts`
    /// skips the feature entirely and emits no RUN step to assert against.
    fn make_feature(id: &str, options: serde_json::Value) -> ResolvedFeature {
        let mut resolved = feature(id);
        resolved.options = options;
        resolved.install_script_path = PathBuf::from("/tmp/fake");
        resolved
    }

    #[test]
    fn feature_options_do_not_leak_across_features() {
        let features = vec![
            make_feature("feature-a", serde_json::json!({"version": "3.12"})),
            make_feature("feature-b", serde_json::json!({})),
        ];
        let config = empty_config();
        let dockerfile =
            generate_feature_dockerfile_with_opts("base:latest", &features, Some("root"), &config);

        // Feature options should be in RUN (scoped), not ENV (global).
        assert!(
            !dockerfile.contains("ENV VERSION="),
            "Feature options must not use ENV directives (they leak across features).\nDockerfile:\n{dockerfile}"
        );
        // feature-a's RUN should contain the export.
        assert!(
            dockerfile.contains("export VERSION=\"$(printf '%b' '3.12')\""),
            "Feature-a's RUN step should export VERSION.\nDockerfile:\n{dockerfile}"
        );
    }

    #[test]
    fn container_env_uses_env_directive() {
        // containerEnv intentionally persists in the image — should use ENV.
        let mut feature = make_feature("feature-a", serde_json::json!({}));
        feature
            .container_env
            .insert("MY_VAR".to_string(), "hello".to_string());
        let features = vec![feature];
        let config = empty_config();
        let dockerfile =
            generate_feature_dockerfile_with_opts("base:latest", &features, Some("root"), &config);
        assert!(
            dockerfile.contains("ENV MY_VAR=\"hello\""),
            "containerEnv should use ENV directives.\nDockerfile:\n{dockerfile}"
        );
    }

    #[test]
    fn option_name_to_env_uppercases() {
        assert_eq!(option_name_to_env("version"), "VERSION");
        assert_eq!(option_name_to_env("nodeVersion"), "NODEVERSION");
        assert_eq!(option_name_to_env("my-option"), "MY_OPTION");
    }

    #[test]
    fn feature_options_escape_special_characters() {
        let features = vec![make_feature(
            "feature-a",
            serde_json::json!({"desc": "line1\nline2"}),
        )];
        let config = empty_config();
        let dockerfile =
            generate_feature_dockerfile_with_opts("base:latest", &features, Some("root"), &config);
        // Newlines should be escaped as \n inside printf, not literal newlines
        // that would break the Dockerfile RUN instruction.
        assert!(
            !dockerfile.contains("line1\nline2"),
            "Literal newlines must not appear in Dockerfile.\nDockerfile:\n{dockerfile}"
        );
        assert!(
            dockerfile.contains("export DESC=\"$(printf '%b' 'line1\\nline2')\""),
            "Special characters should be escaped via printf.\nDockerfile:\n{dockerfile}"
        );
    }
}
