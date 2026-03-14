use sha2::{Digest, Sha256};
use std::path::Path;

/// Normalize a string for use as a Docker image name/tag.
///
/// Matches the official devcontainer CLI's `toDockerImageName()`:
/// - Lowercases the input
/// - Removes any character not in `[a-z0-9._-]`
/// - Cleans separator sequences (collapses runs of `._-` chars)
fn normalize_docker_image_name(name: &str) -> String {
    // Pass 1: lowercase and keep only [a-z0-9._-]
    let pass1: String = name
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '.' || *ch == '_' || *ch == '-')
        .collect();

    // Pass 2: clean separator sequences to match the official regex:
    //   /(\.[\._-]|_[\.-]|__[\._-]|-+[\._])[\._-]*/g
    // Replacement: captured group minus its last character.
    let bytes = pass1.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    fn is_sep(b: u8) -> bool {
        b == b'.' || b == b'_' || b == b'-'
    }

    while i < len {
        if !is_sep(bytes[i]) {
            result.push(bytes[i] as char);
            i += 1;
            continue;
        }

        // Try to match one of the regex alternatives at position i.
        if let Some((group_len, total_len)) = try_sep_match(bytes, i) {
            // Emit group[..group_len-1] (all but the last char of captured group)
            for &b in &bytes[i..i + group_len - 1] {
                result.push(b as char);
            }
            i += total_len;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

/// Try to match one of the separator-cleaning regex alternatives at position `i`.
/// Returns `Some((group_len, total_len))` where `group_len` is the captured
/// group length and `total_len` includes trailing separators consumed.
fn try_sep_match(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let len = bytes.len();

    fn is_sep(b: u8) -> bool {
        b == b'.' || b == b'_' || b == b'-'
    }

    fn trailing_seps(bytes: &[u8], start: usize) -> usize {
        let mut n = 0;
        while start + n < bytes.len() && is_sep(bytes[start + n]) {
            n += 1;
        }
        n
    }

    // 1. \.[\._-]  — dot followed by any separator
    if bytes[i] == b'.' && i + 1 < len && is_sep(bytes[i + 1]) {
        let t = trailing_seps(bytes, i + 2);
        return Some((2, 2 + t));
    }

    // 2. _[\.-]  — underscore followed by dot or dash
    if bytes[i] == b'_' && i + 1 < len && (bytes[i + 1] == b'.' || bytes[i + 1] == b'-') {
        let t = trailing_seps(bytes, i + 2);
        return Some((2, 2 + t));
    }

    // 3. __[\._-]  — two underscores followed by any separator
    if bytes[i] == b'_'
        && i + 1 < len
        && bytes[i + 1] == b'_'
        && i + 2 < len
        && is_sep(bytes[i + 2])
    {
        let t = trailing_seps(bytes, i + 3);
        return Some((3, 3 + t));
    }

    // 4. -+[\._]  — one or more dashes followed by dot or underscore
    if bytes[i] == b'-' {
        let mut j = i;
        while j < len && bytes[j] == b'-' {
            j += 1;
        }
        if j < len && (bytes[j] == b'.' || bytes[j] == b'_') {
            let group_len = j - i + 1;
            let t = trailing_seps(bytes, i + group_len);
            return Some((group_len, group_len + t));
        }
    }

    None
}

/// Generate a deterministic container name for a workspace.
///
/// Format: `vsc-<dirname>-<SHA-256 hex of absolute path>`
///
/// This matches the naming convention used by VS Code's Dev Containers extension
/// so that containers are interoperable between the two tools.
pub fn container_name(workspace: &Path) -> String {
    let abs_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let path_str = abs_path.to_string_lossy();

    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let hash = hex::encode(hasher.finalize());

    let dirname = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    let normalized = normalize_docker_image_name(&dirname);
    format!("vsc-{normalized}-{hash}")
}

/// Return label key-value pairs used to identify containers belonging to a workspace.
///
/// Uses the official devcontainer CLI labels:
/// - `devcontainer.local_folder=<absolute workspace path>`
/// - `devcontainer.config_file=<absolute config file path>` (when provided)
pub fn workspace_labels(workspace: &Path, config_file: Option<&Path>) -> Vec<(String, String)> {
    let abs_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut labels = vec![(
        "devcontainer.local_folder".to_string(),
        abs_path.to_string_lossy().to_string(),
    )];
    if let Some(cf) = config_file {
        let abs_cf = cf.canonicalize().unwrap_or_else(|_| cf.to_path_buf());
        labels.push((
            "devcontainer.config_file".to_string(),
            abs_cf.to_string_lossy().to_string(),
        ));
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_container_name_format() {
        // Use a path that won't canonicalize (doesn't exist), so it uses the raw path.
        let name = container_name(Path::new("/tmp/my-project"));
        assert!(name.starts_with("vsc-my-project-"));
        // vsc- (4) + my-project (10) + - (1) + 64-char hex hash = 79
        assert_eq!(name.len(), 79);
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
    fn test_container_name_normalizes_special_chars() {
        let name = container_name(Path::new("/tmp/My Project@v2!"));
        // Special chars removed (not replaced), per official CLI
        assert!(name.starts_with("vsc-myprojectv2-"));
        assert_eq!(name, name.to_lowercase());
    }

    #[test]
    fn test_normalize_docker_image_name() {
        assert_eq!(normalize_docker_image_name("My-Project"), "my-project");
        // Non-[a-z0-9._-] chars are removed entirely
        assert_eq!(normalize_docker_image_name("foo@bar!baz"), "foobarbaz");
        assert_eq!(normalize_docker_image_name("My Project@v2!"), "myprojectv2");
        // Dots and underscores are preserved
        assert_eq!(normalize_docker_image_name("my_project.v2"), "my_project.v2");
        // Separator sequences are cleaned per official regex
        assert_eq!(normalize_docker_image_name("my..project"), "my.project");
        assert_eq!(normalize_docker_image_name("a._-b"), "a.b");
        assert_eq!(normalize_docker_image_name("simple"), "simple");
        // Consecutive dashes are NOT collapsed (regex doesn't match dash-only runs)
        assert_eq!(normalize_docker_image_name("--leading--"), "--leading--");
        // Underscore followed by dot or dash: cleaned
        assert_eq!(normalize_docker_image_name("a_-b"), "a_b");
        assert_eq!(normalize_docker_image_name("a_.b"), "a_b");
        // Dashes followed by dot or underscore: cleaned
        assert_eq!(normalize_docker_image_name("a-_b"), "a-b");
        assert_eq!(normalize_docker_image_name("a-.b"), "a-b");
        // Triple underscore: __[\._-] matches, keeps __
        assert_eq!(normalize_docker_image_name("a___b"), "a__b");
    }

    #[test]
    fn test_workspace_labels_without_config() {
        let labels = workspace_labels(Path::new("/tmp/my-project"), None);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].0, "devcontainer.local_folder");
    }

    #[test]
    fn test_workspace_labels_with_config() {
        let labels = workspace_labels(
            Path::new("/tmp/my-project"),
            Some(Path::new("/tmp/my-project/.devcontainer/devcontainer.json")),
        );
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].0, "devcontainer.local_folder");
        assert_eq!(labels[1].0, "devcontainer.config_file");
    }
}
