use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::DevError;
use crate::oci::{download_artifact, extract_archive, sha256_hex};

use super::config::{DevcontainerConfig, LifecycleCommand};

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

/// Classify a feature reference string into its kind.
fn classify_feature_ref(id: &str) -> FeatureRefKind {
    if id.starts_with("./") || id.starts_with("../") {
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
    for i in 0..features.len() {
        download_single_feature(&mut features[i], devcontainer_dir).await?;
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
        FeatureRefKind::Tarball(url) => {
            download_tarball_feature(&url).await?
        }
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
        let stripped = json_comments::StripComments::new(content.as_bytes());
        let meta: FeatureJsonMeta = serde_json::from_reader(stripped)?;
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
    let meta_path = feature.install_script_path.join("devcontainer-feature.json");
    if !meta_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&meta_path).ok()?;
    let stripped = json_comments::StripComments::new(content.as_bytes());
    let meta: FeatureJsonMeta = serde_json::from_reader(stripped).ok()?;
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
            if let Some(deps) = deps_cache.get(&f.id) {
                if deps.contains_key(&dep_id) {
                    f.install_after.push(dep_id.clone());
                }
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
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let trimmed = sanitized.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let result = if trimmed.is_empty() { &sanitized } else { trimmed };
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
/// Gap 16 fix: When `buildkit` is true, uses `RUN --mount=type=bind` for zero-copy install.
/// Like [`generate_feature_dockerfile`] but with an option to enable BuildKit optimizations.
pub fn generate_feature_dockerfile_with_opts(
    base_image: &str,
    features: &[ResolvedFeature],
    remote_user: Option<&str>,
    config: &DevcontainerConfig,
    buildkit: bool,
) -> String {
    let user = remote_user.unwrap_or("root");

    let mut lines: Vec<String> = Vec::new();

    // BuildKit syntax directive must be the very first line.
    if buildkit {
        lines.push("# syntax=docker/dockerfile:1".to_string());
    }

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
        for (key, val) in &feature.container_env {
            let escaped_val = val.replace('\\', "\\\\").replace('"', "\\\"");
            lines.push(format!("ENV {key}=\"{escaped_val}\""));
        }

        // Pass feature options as environment variables (uppercased per spec).
        if let Some(obj) = feature.options.as_object() {
            for (key, val) in obj {
                let env_name = option_name_to_env(key);
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                lines.push(format!("ENV {env_name}={val_str}"));
            }
        }

        if buildkit {
            // Gap 16: Use BuildKit bind mount for zero-copy feature install.
            // The feature files are bind-mounted from the build context directly,
            // avoiding the ADD + extract overhead and reducing layer size.
            lines.push(format!(
                "RUN --mount=type=bind,source={i}.tar,target=/tmp/_dev_feature_{i}.tar \
                 mkdir -p {stage_dir} && \
                 tar xf /tmp/_dev_feature_{i}.tar -C {stage_dir} && \
                 {wrapper}",
                wrapper = feature_wrapper_script(&feature.id, &feature.version, &stage_dir),
            ));
        } else {
            lines.push(format!("ADD {i}.tar {stage_dir}/"));

            // Gap 12: Wrapper script with env sourcing and error context.
            lines.push(format!(
                "RUN {wrapper}",
                wrapper = feature_wrapper_script(&feature.id, &feature.version, &stage_dir),
            ));
        }
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
/// 2. Sets error context variables (_DEV_FEATURE_ID, _DEV_FEATURE_VERSION)
/// 3. Runs install.sh with `set -e` for proper error propagation
/// 4. Reports clear error messages on failure
fn feature_wrapper_script(feature_id: &str, feature_version: &str, stage_dir: &str) -> String {
    // Shell-escape the feature ID for safe embedding in the script.
    let escaped_id = feature_id.replace('\'', "'\\''");
    let escaped_version = feature_version.replace('\'', "'\\''");
    format!(
        "set -e && \
         if [ -f /usr/local/share/dev-container-user-home.sh ]; then \
           . /usr/local/share/dev-container-user-home.sh; \
         fi && \
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
        insert_lifecycle_hook(&mut entry, "onCreateCommand", &feature.lifecycle_hooks.on_create_command);
        insert_lifecycle_hook(&mut entry, "postCreateCommand", &feature.lifecycle_hooks.post_create_command);
        insert_lifecycle_hook(&mut entry, "postStartCommand", &feature.lifecycle_hooks.post_start_command);
        insert_lifecycle_hook(&mut entry, "postAttachCommand", &feature.lifecycle_hooks.post_attach_command);

        metadata.push(serde_json::Value::Object(entry));
    }

    // Base config entry.
    let mut base_entry = serde_json::Map::new();
    if let Some(user) = remote_user {
        base_entry.insert("remoteUser".into(), serde_json::Value::String(user.to_string()));
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
            LifecycleCommand::Multiple(arr) => {
                serde_json::Value::Array(arr.iter().map(|s| serde_json::Value::String(s.clone())).collect())
            }
            LifecycleCommand::Parallel(map) => {
                serde_json::to_value(map).unwrap_or_default()
            }
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
#[derive(Debug, Default)]
pub struct MergedCapabilities {
    pub init: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
}

