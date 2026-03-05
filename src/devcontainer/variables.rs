use std::path::Path;

/// Expand devcontainer variables in a string.
///
/// Supported variables:
/// - `${localEnv:VAR}` — value of host env var, empty string if unset
/// - `${localEnv:VAR:default}` — value of host env var, `default` if unset
/// - `${containerEnv:VAR}` — left as-is (resolved at container runtime)
/// - `${localWorkspaceFolder}` — workspace path on host
/// - `${localWorkspaceFolderBasename}` — basename of workspace path
pub fn substitute_variables(s: &str, workspace: &Path) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            // Find the closing brace
            if let Some(close) = s[i..].find('}') {
                let expr = &s[i + 2..i + close]; // content between ${ and }
                if let Some(expanded) = expand_variable(expr, workspace) {
                    result.push_str(&expanded);
                    i += close + 1;
                    continue;
                }
            }
        }
        result.push(s[i..].chars().next().unwrap());
        i += s[i..].chars().next().unwrap().len_utf8();
    }

    result
}

/// Try to expand a single variable expression (the content between `${` and `}`).
/// Returns `None` if the expression is not a recognised variable (leave as-is).
fn expand_variable(expr: &str, workspace: &Path) -> Option<String> {
    if let Some(rest) = expr.strip_prefix("localEnv:") {
        // rest is either "VAR" or "VAR:default"
        let (var_name, default) = match rest.find(':') {
            Some(pos) => (&rest[..pos], &rest[pos + 1..]),
            None => (rest, ""),
        };
        Some(std::env::var(var_name).unwrap_or_else(|_| default.to_string()))
    } else if expr.starts_with("containerEnv:") {
        // Leave containerEnv variables as-is — they are resolved at runtime
        None
    } else if expr == "localWorkspaceFolder" {
        Some(workspace.to_string_lossy().into_owned())
    } else if expr == "localWorkspaceFolderBasename" {
        Some(
            workspace
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_no_variables() {
        let workspace = PathBuf::from("/home/user/project");
        assert_eq!(substitute_variables("hello world", &workspace), "hello world");
    }

    #[test]
    fn test_local_env_set() {
        let workspace = PathBuf::from("/home/user/project");
        // SAFETY: test-only; tests run serially with --test-threads=1 or use unique var names.
        unsafe { std::env::set_var("DEV_TEST_VAR", "/tmp/test") };
        assert_eq!(
            substitute_variables("${localEnv:DEV_TEST_VAR}/file", &workspace),
            "/tmp/test/file"
        );
        unsafe { std::env::remove_var("DEV_TEST_VAR") };
    }

    #[test]
    fn test_local_env_unset() {
        let workspace = PathBuf::from("/home/user/project");
        unsafe { std::env::remove_var("DEV_TEST_NONEXISTENT") };
        assert_eq!(
            substitute_variables("pre-${localEnv:DEV_TEST_NONEXISTENT}-post", &workspace),
            "pre--post"
        );
    }

    #[test]
    fn test_local_env_default() {
        let workspace = PathBuf::from("/home/user/project");
        unsafe { std::env::remove_var("DEV_TEST_NONEXISTENT2") };
        assert_eq!(
            substitute_variables("${localEnv:DEV_TEST_NONEXISTENT2:fallback}", &workspace),
            "fallback"
        );
    }

    #[test]
    fn test_local_env_set_ignores_default() {
        let workspace = PathBuf::from("/home/user/project");
        unsafe { std::env::set_var("DEV_TEST_VAR2", "actual") };
        assert_eq!(
            substitute_variables("${localEnv:DEV_TEST_VAR2:fallback}", &workspace),
            "actual"
        );
        unsafe { std::env::remove_var("DEV_TEST_VAR2") };
    }

    #[test]
    fn test_container_env_left_as_is() {
        let workspace = PathBuf::from("/home/user/project");
        assert_eq!(
            substitute_variables("${containerEnv:PATH}", &workspace),
            "${containerEnv:PATH}"
        );
    }

    #[test]
    fn test_local_workspace_folder() {
        let workspace = PathBuf::from("/home/user/project");
        assert_eq!(
            substitute_variables("${localWorkspaceFolder}/sub", &workspace),
            "/home/user/project/sub"
        );
    }

    #[test]
    fn test_local_workspace_folder_basename() {
        let workspace = PathBuf::from("/home/user/project");
        assert_eq!(
            substitute_variables("/workspaces/${localWorkspaceFolderBasename}", &workspace),
            "/workspaces/project"
        );
    }

    #[test]
    fn test_multiple_variables() {
        let workspace = PathBuf::from("/home/user/project");
        unsafe { std::env::set_var("DEV_TEST_MULTI", "value") };
        assert_eq!(
            substitute_variables(
                "source=${localEnv:DEV_TEST_MULTI},target=/workspaces/${localWorkspaceFolderBasename}",
                &workspace
            ),
            "source=value,target=/workspaces/project"
        );
        unsafe { std::env::remove_var("DEV_TEST_MULTI") };
    }

    #[test]
    fn test_mount_string_with_local_env() {
        let workspace = PathBuf::from("/home/user/project");
        unsafe { std::env::set_var("DEV_TEST_HOME", "/home/user") };
        let input = "source=${localEnv:DEV_TEST_HOME}/.config/omp/theme.json,target=/home/vscode/.config/omp/theme.json,type=bind";
        let expected = "source=/home/user/.config/omp/theme.json,target=/home/vscode/.config/omp/theme.json,type=bind";
        assert_eq!(substitute_variables(input, &workspace), expected);
        unsafe { std::env::remove_var("DEV_TEST_HOME") };
    }
}
