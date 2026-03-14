use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::DevError;
use super::features::ResolvedFeature;

/// A lockfile entry for a single feature, pinning its version and content digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedFeature {
    /// The resolved OCI version tag (e.g., "1", "1.2.3", "latest").
    pub version: String,
    /// SHA-256 digest of the downloaded artifact blob.
    pub integrity: String,
}

/// The full lockfile structure written to `devcontainer-lock.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lockfile {
    pub features: HashMap<String, LockedFeature>,
}

impl Lockfile {
    /// Read a lockfile from disk. Returns `None` if the file doesn't exist.
    pub fn from_path(path: &Path) -> Result<Option<Self>, DevError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let lockfile: Lockfile = serde_json::from_str(&content)?;
        Ok(Some(lockfile))
    }

    /// Write the lockfile to disk.
    pub fn write(&self, path: &Path) -> Result<(), DevError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| DevError::InvalidConfig(format!("Failed to serialize lockfile: {e}")))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Build a lockfile from resolved features after download.
    pub fn from_features(features: &[ResolvedFeature]) -> Self {
        let mut locked = HashMap::new();
        for f in features {
            if f.install_script_path.as_os_str().is_empty() {
                continue;
            }
            let integrity = compute_feature_integrity(&f.install_script_path);
            locked.insert(
                f.id.clone(),
                LockedFeature {
                    version: f.version.clone(),
                    integrity,
                },
            );
        }
        Lockfile { features: locked }
    }

    /// Verify that downloaded features match the lockfile entries.
    /// Returns a list of feature IDs that don't match.
    pub fn verify(&self, features: &[ResolvedFeature]) -> Vec<String> {
        let mut mismatches = Vec::new();
        for f in features {
            if f.install_script_path.as_os_str().is_empty() {
                continue;
            }
            if let Some(locked) = self.features.get(&f.id) {
                let actual = compute_feature_integrity(&f.install_script_path);
                if actual != locked.integrity {
                    mismatches.push(f.id.clone());
                }
            } else {
                // Feature not in lockfile — new addition
                mismatches.push(f.id.clone());
            }
        }
        mismatches
    }
}

/// Compute a SHA-256 integrity hash for a feature's extracted directory.
/// Hashes all file contents in sorted order for determinism.
fn compute_feature_integrity(dir: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut paths: Vec<PathBuf> = Vec::new();

    collect_files(dir, &mut paths);
    paths.sort();

    for path in &paths {
        // Hash the relative path
        if let Ok(rel) = path.strip_prefix(dir) {
            hasher.update(rel.to_string_lossy().as_bytes());
        }
        // Hash the file contents
        if let Ok(content) = std::fs::read(path) {
            hasher.update(&content);
        }
    }

    let result = hasher.finalize();
    hex::encode(result)
}

/// Recursively collect all file paths in a directory.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}

/// Resolve the lockfile path relative to the devcontainer directory.
pub fn lockfile_path(devcontainer_dir: &Path) -> PathBuf {
    devcontainer_dir.join("devcontainer-lock.json")
}

/// Expose `compute_feature_integrity` for testing.
#[cfg(test)]
pub(crate) fn test_compute_integrity(dir: &Path) -> String {
    compute_feature_integrity(dir)
}

/// Read/write/verify the lockfile, erroring in frozen mode if mismatched or absent.
pub fn handle_lockfile(
    lf_path: &Path,
    features: &[ResolvedFeature],
    frozen: bool,
) -> Result<(), DevError> {
    match Lockfile::from_path(lf_path)? {
        Some(existing) => {
            let mismatches = existing.verify(features);
            if !mismatches.is_empty() {
                if frozen {
                    return Err(DevError::InvalidConfig(format!(
                        "Lockfile mismatch for features: {}. \
                         Run without --frozen-lockfile to update.",
                        mismatches.join(", ")
                    )));
                }
                eprintln!(
                    "Lockfile outdated for {} feature(s), updating...",
                    mismatches.len()
                );
                let new_lockfile = Lockfile::from_features(features);
                new_lockfile.write(lf_path)?;
            }
        }
        None => {
            if frozen {
                return Err(DevError::InvalidConfig(format!(
                    "No lockfile found at {}. \
                     Run without --frozen-lockfile to generate one.",
                    lf_path.display()
                )));
            }
            eprintln!("Writing lockfile to {}", lf_path.display());
            let lockfile = Lockfile::from_features(features);
            lockfile.write(lf_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::features::FeatureLifecycleHooks;
    use std::fs;
    use tempfile::TempDir;

    /// Build a minimal ResolvedFeature pointing at `install_script_path`.
    fn make_feature(id: &str, version: &str, path: PathBuf) -> ResolvedFeature {
        ResolvedFeature {
            id: id.to_string(),
            oci_ref: id.to_string(),
            version: version.to_string(),
            options: serde_json::Value::Null,
            install_script_path: path,
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

    /// Create a temp feature directory with an install.sh containing `content`.
    fn make_feature_dir(tmp: &TempDir, name: &str, content: &str) -> PathBuf {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("install.sh"), content).unwrap();
        dir
    }

    // ── from_path ──────────────────────────────────────────────

    #[test]
    fn test_from_path_missing_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let result = Lockfile::from_path(&tmp.path().join("does-not-exist.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_from_path_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lock.json");
        fs::write(
            &path,
            r#"{"features":{"node":{"version":"1","integrity":"abc123"}}}"#,
        )
        .unwrap();

        let lockfile = Lockfile::from_path(&path).unwrap().unwrap();
        assert_eq!(lockfile.features.len(), 1);
        let node = &lockfile.features["node"];
        assert_eq!(node.version, "1");
        assert_eq!(node.integrity, "abc123");
    }

    #[test]
    fn test_from_path_invalid_json_returns_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        fs::write(&path, "not json").unwrap();

        let result = Lockfile::from_path(&path);
        assert!(result.is_err());
    }

    // ── write + roundtrip ──────────────────────────────────────

    #[test]
    fn test_write_and_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lock.json");

        let mut features = HashMap::new();
        features.insert(
            "feat-a".to_string(),
            LockedFeature {
                version: "2.0".to_string(),
                integrity: "deadbeef".to_string(),
            },
        );
        let lockfile = Lockfile { features };
        lockfile.write(&path).unwrap();

        let loaded = Lockfile::from_path(&path).unwrap().unwrap();
        assert_eq!(loaded.features.len(), 1);
        assert_eq!(loaded.features["feat-a"].version, "2.0");
        assert_eq!(loaded.features["feat-a"].integrity, "deadbeef");
    }

    // ── from_features ──────────────────────────────────────────

    #[test]
    fn test_from_features_skips_empty_path() {
        let features = vec![make_feature("skipped", "1", PathBuf::new())];
        let lockfile = Lockfile::from_features(&features);
        assert!(lockfile.features.is_empty());
    }

    #[test]
    fn test_from_features_builds_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = make_feature_dir(&tmp, "node", "echo install node");
        let features = vec![make_feature("node", "1.2.3", dir)];

        let lockfile = Lockfile::from_features(&features);
        assert_eq!(lockfile.features.len(), 1);
        let entry = &lockfile.features["node"];
        assert_eq!(entry.version, "1.2.3");
        assert!(!entry.integrity.is_empty());
    }

    // ── verify ─────────────────────────────────────────────────

    #[test]
    fn test_verify_matching_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = make_feature_dir(&tmp, "node", "echo install");
        let features = vec![make_feature("node", "1", dir)];

        let lockfile = Lockfile::from_features(&features);
        let mismatches = lockfile.verify(&features);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_verify_detects_changed_content() {
        let tmp = TempDir::new().unwrap();
        let dir = make_feature_dir(&tmp, "node", "echo original");
        let features = vec![make_feature("node", "1", dir.clone())];
        let lockfile = Lockfile::from_features(&features);

        // Mutate the file on disk
        fs::write(dir.join("install.sh"), "echo tampered").unwrap();
        let mismatches = lockfile.verify(&features);
        assert_eq!(mismatches, vec!["node"]);
    }

    #[test]
    fn test_verify_detects_new_feature() {
        let tmp = TempDir::new().unwrap();
        let dir_a = make_feature_dir(&tmp, "a", "echo a");
        let dir_b = make_feature_dir(&tmp, "b", "echo b");

        // Lockfile only knows about feature "a"
        let features_a = vec![make_feature("a", "1", dir_a.clone())];
        let lockfile = Lockfile::from_features(&features_a);

        // Now verify against both "a" and "b"
        let features_ab = vec![
            make_feature("a", "1", dir_a),
            make_feature("b", "1", dir_b),
        ];
        let mismatches = lockfile.verify(&features_ab);
        assert_eq!(mismatches, vec!["b"]);
    }

    #[test]
    fn test_verify_skips_empty_path() {
        let lockfile = Lockfile {
            features: HashMap::new(),
        };
        let features = vec![make_feature("skipped", "1", PathBuf::new())];
        let mismatches = lockfile.verify(&features);
        assert!(mismatches.is_empty());
    }

    // ── compute_feature_integrity ──────────────────────────────

    #[test]
    fn test_integrity_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        let dir = make_feature_dir(&tmp, "feat", "echo hello");
        let h1 = test_compute_integrity(&dir);
        let h2 = test_compute_integrity(&dir);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_integrity_changes_with_content() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("feat");
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("install.sh"), "version1").unwrap();
        let h1 = test_compute_integrity(&dir);

        fs::write(dir.join("install.sh"), "version2").unwrap();
        let h2 = test_compute_integrity(&dir);

        assert_ne!(h1, h2);
    }

    #[test]
    fn test_integrity_includes_nested_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("feat");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("install.sh"), "main").unwrap();

        let h_without = test_compute_integrity(&dir);

        fs::write(dir.join("sub/helper.sh"), "helper").unwrap();
        let h_with = test_compute_integrity(&dir);

        assert_ne!(h_without, h_with);
    }

    // ── lockfile_path ──────────────────────────────────────────

    #[test]
    fn test_lockfile_path() {
        let p = lockfile_path(Path::new("/project/.devcontainer"));
        assert_eq!(p, PathBuf::from("/project/.devcontainer/devcontainer-lock.json"));
    }

    // ── handle_lockfile ────────────────────────────────────────

    #[test]
    fn test_handle_lockfile_creates_when_absent() {
        let tmp = TempDir::new().unwrap();
        let lf_path = tmp.path().join("lock.json");
        let dir = make_feature_dir(&tmp, "feat", "echo hi");
        let features = vec![make_feature("feat", "1", dir)];

        handle_lockfile(&lf_path, &features, false).unwrap();
        assert!(lf_path.exists());

        let lockfile = Lockfile::from_path(&lf_path).unwrap().unwrap();
        assert!(lockfile.features.contains_key("feat"));
    }

    #[test]
    fn test_handle_lockfile_frozen_errors_when_absent() {
        let tmp = TempDir::new().unwrap();
        let lf_path = tmp.path().join("lock.json");
        let features = vec![make_feature("feat", "1", PathBuf::new())];

        let result = handle_lockfile(&lf_path, &features, true);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No lockfile found"));
    }

    #[test]
    fn test_handle_lockfile_updates_when_mismatched() {
        let tmp = TempDir::new().unwrap();
        let lf_path = tmp.path().join("lock.json");
        let dir = make_feature_dir(&tmp, "feat", "echo original");
        let features = vec![make_feature("feat", "1", dir.clone())];

        // Create initial lockfile
        handle_lockfile(&lf_path, &features, false).unwrap();

        // Change the feature content
        fs::write(dir.join("install.sh"), "echo changed").unwrap();

        // Should update (not error)
        handle_lockfile(&lf_path, &features, false).unwrap();

        // Verify the lockfile was updated to match the new content
        let updated = Lockfile::from_path(&lf_path).unwrap().unwrap();
        let mismatches = updated.verify(&features);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_handle_lockfile_frozen_errors_when_mismatched() {
        let tmp = TempDir::new().unwrap();
        let lf_path = tmp.path().join("lock.json");
        let dir = make_feature_dir(&tmp, "feat", "echo original");
        let features = vec![make_feature("feat", "1", dir.clone())];

        handle_lockfile(&lf_path, &features, false).unwrap();

        // Change the feature content
        fs::write(dir.join("install.sh"), "echo changed").unwrap();

        let result = handle_lockfile(&lf_path, &features, true);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Lockfile mismatch"));
    }

    #[test]
    fn test_handle_lockfile_noop_when_matching() {
        let tmp = TempDir::new().unwrap();
        let lf_path = tmp.path().join("lock.json");
        let dir = make_feature_dir(&tmp, "feat", "echo stable");
        let features = vec![make_feature("feat", "1", dir)];

        handle_lockfile(&lf_path, &features, false).unwrap();
        let mtime_before = fs::metadata(&lf_path).unwrap().modified().unwrap();

        // Small delay to ensure mtime would differ if the file were rewritten
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Should succeed without rewriting
        handle_lockfile(&lf_path, &features, false).unwrap();
        let mtime_after = fs::metadata(&lf_path).unwrap().modified().unwrap();

        assert_eq!(mtime_before, mtime_after);
    }
}
