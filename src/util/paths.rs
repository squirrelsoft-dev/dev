use std::fs;
use std::path::{Path, PathBuf};

/// Root of user-level dev config: `~/.dev/`
pub fn dev_home() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".dev")
}

/// Global templates directory: `~/.dev/global/`
pub fn global_dir() -> PathBuf {
    dev_home().join("global")
}

/// User-scoped project devcontainers: `~/.dev/devcontainers/`
pub fn devcontainers_dir() -> PathBuf {
    dev_home().join("devcontainers")
}

/// Base config directory: `~/.dev/base/`
pub fn base_config_dir() -> PathBuf {
    dev_home().join("base")
}

/// Runtime-specific config directory: `~/.dev/<name>/`
pub fn runtime_config_dir(name: &str) -> PathBuf {
    dev_home().join(name)
}

/// VS Code remote-containers configs directory.
///
/// - macOS: `~/Library/Application Support/Code/User/globalStorage/ms-vscode-remote.remote-containers/configs/`
/// - Linux: `~/.config/Code/User/globalStorage/ms-vscode-remote.remote-containers/configs/`
pub fn vscode_configs_dir() -> PathBuf {
    dirs::config_dir()
        .expect("could not determine config directory")
        .join("Code/User/globalStorage/ms-vscode-remote.remote-containers/configs")
}

/// Create a symlink from VS Code's remote-containers `configs/` dir to the
/// user-scoped config directory.
///
/// This makes the config visible via "Open Named Container Configuration" in
/// VS Code and keeps it accessible for `dev vscode repair`.
///
/// Silently skips if VS Code's remote-containers extension is not installed
/// or the symlink already points to the correct target.
pub fn create_vscode_symlink(folder_name: &str, target: &Path) {
    let vscode_dir = vscode_configs_dir();
    if !vscode_dir.parent().map(|p| p.is_dir()).unwrap_or(false) {
        // VS Code remote-containers extension not installed
        return;
    }

    let _ = fs::create_dir_all(&vscode_dir);
    let link_path = vscode_dir.join(folder_name);

    // Check if a valid symlink already points to the correct target.
    #[cfg(unix)]
    {
        if let Ok(existing_target) = std::fs::read_link(&link_path) {
            if existing_target == target {
                return;
            }
            // Symlink exists but points elsewhere (stale/broken) — remove it.
            let _ = fs::remove_file(&link_path);
        } else if link_path.exists() {
            // Not a symlink but something else exists — don't overwrite.
            return;
        }

        let _ = std::os::unix::fs::symlink(target, &link_path);
    }

    #[cfg(not(unix))]
    {
        let _ = link_path;
        let _ = target;
    }
}
