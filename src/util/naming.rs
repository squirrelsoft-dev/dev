use sha2::{Digest, Sha256};
use std::path::Path;

/// Generate a deterministic container name for a workspace.
///
/// Format: `dev-<first 8 chars of SHA-256 of absolute path>-<dirname>`
pub fn container_name(workspace: &Path) -> String {
    let abs_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let path_str = abs_path.to_string_lossy();

    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let short_hash = &hash[..8];

    let dirname = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    format!("dev-{short_hash}-{}", dirname.to_lowercase())
}

/// Return the label key-value pair used to identify containers belonging to a workspace.
///
/// Returns `("dev.workspace.path", <absolute path string>)`.
pub fn workspace_label(workspace: &Path) -> (String, String) {
    let abs_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    (
        "dev.workspace.path".to_string(),
        abs_path.to_string_lossy().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_container_name_format() {
        // Use a path that won't canonicalize (doesn't exist), so it uses the raw path.
        let name = container_name(Path::new("/tmp/my-project"));
        assert!(name.starts_with("dev-"));
        assert!(name.ends_with("-my-project"));
        assert_eq!(name.len(), 23);
        // Verify name is fully lowercase (Docker requirement)
        assert_eq!(name, name.to_lowercase());
    }

    #[test]
    fn test_container_name_deterministic() {
        let a = container_name(Path::new("/tmp/test-workspace"));
        let b = container_name(Path::new("/tmp/test-workspace"));
        assert_eq!(a, b);
    }

    #[test]
    fn test_workspace_label() {
        let (key, _) = workspace_label(Path::new("/tmp/my-project"));
        assert_eq!(key, "dev.workspace.path");
    }
}
