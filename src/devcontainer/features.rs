use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::DevError;
use crate::oci::download_artifact;

use super::config::DevcontainerConfig;

/// Metadata from `devcontainer-feature.json` inside a feature artifact.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FeatureJsonMeta {
    #[serde(default)]
    install_after: Option<Vec<String>>,
}

/// A resolved devcontainer feature ready for installation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedFeature {
    pub id: String,
    pub oci_ref: String,
    pub version: String,
    pub options: serde_json::Value,
    pub install_script_path: PathBuf,
    /// Features that must be installed before this one.
    pub install_after: Vec<String>,
}

/// Parse feature references from the config and resolve them into installable features.
pub fn resolve_features(config: &DevcontainerConfig) -> Result<Vec<ResolvedFeature>, DevError> {
    let features = match &config.features {
        Some(f) if !f.is_empty() => f,
        _ => return Ok(Vec::new()),
    };

    let mut resolved = Vec::new();
    for (id, options) in features {
        let (oci_ref, version) = parse_feature_ref(id);
        resolved.push(ResolvedFeature {
            id: id.clone(),
            oci_ref,
            version,
            options: options.clone(),
            install_script_path: PathBuf::new(), // Set after download
            install_after: Vec::new(),
        });
    }

    Ok(resolved)
}

/// Download OCI artifacts for each feature, populating `install_script_path` and `install_after`.
pub async fn download_features(features: &mut [ResolvedFeature]) -> Result<(), DevError> {
    for feature in features.iter_mut() {
        let extracted_dir = download_artifact(&feature.oci_ref, &feature.version).await?;

        // Verify install.sh exists
        let install_sh = extracted_dir.join("install.sh");
        if !install_sh.exists() {
            return Err(DevError::FeatureNotFound(format!(
                "install.sh not found in feature '{}'",
                feature.id
            )));
        }

        // Read optional devcontainer-feature.json for dependency info
        let meta_path = extracted_dir.join("devcontainer-feature.json");
        if meta_path.exists() {
            let content = std::fs::read_to_string(&meta_path)?;
            let stripped = json_comments::StripComments::new(content.as_bytes());
            let meta: FeatureJsonMeta = serde_json::from_reader(stripped)?;
            if let Some(install_after) = meta.install_after {
                feature.install_after = install_after;
            }
        }

        feature.install_script_path = extracted_dir;
    }
    Ok(())
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
pub fn generate_feature_dockerfile(
    base_image: &str,
    features: &[ResolvedFeature],
    remote_user: Option<&str>,
) -> String {
    let user = remote_user.unwrap_or("root");
    let user_home = if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    };

    let mut lines = vec![format!("FROM {base_image}")];

    // Required environment variables per the devcontainer feature spec.
    lines.push(format!("ENV _REMOTE_USER=\"{user}\""));
    lines.push(format!("ENV _REMOTE_USER_HOME=\"{user_home}\""));
    lines.push(format!("ENV _CONTAINER_USER=\"{user}\""));
    lines.push(format!("ENV _CONTAINER_USER_HOME=\"{user_home}\""));

    for (i, feature) in features.iter().enumerate() {
        let stage_dir = format!("/tmp/dev-features/{i}");
        if feature.install_script_path.as_os_str().is_empty() {
            continue;
        }
        lines.push(format!("ADD {i}.tar {stage_dir}/"));

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

        lines.push(format!("RUN cd {stage_dir} && chmod +x install.sh && ./install.sh"));
    }

    lines.join("\n")
}
