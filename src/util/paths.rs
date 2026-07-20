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

/// The `~/.dev/` layout resolved against an explicit root.
///
/// The free functions above resolve against the real home directory, which makes
/// the code that reads this layout untestable without touching the user's own
/// `~/.dev/`. Taking the root as a value lets composition run against a temporary
/// tree in tests while production keeps using [`DevHome::current`].
#[derive(Debug, Clone)]
pub struct DevHome {
    root: PathBuf,
}

impl DevHome {
    /// The real `~/.dev/`.
    pub fn current() -> Self {
        DevHome { root: dev_home() }
    }

    /// A `~/.dev/`-shaped layout rooted at an arbitrary directory.
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        DevHome { root: root.into() }
    }

    /// A global template's `.devcontainer/devcontainer.json`.
    pub fn global_template_config(&self, template: &str) -> PathBuf {
        self.root
            .join("global")
            .join(template)
            .join(".devcontainer/devcontainer.json")
    }

    /// A global template's `.devcontainer/` directory.
    pub fn global_template_dir(&self, template: &str) -> PathBuf {
        self.root
            .join("global")
            .join(template)
            .join(".devcontainer")
    }

    /// The base config file, `~/.dev/base/devcontainer.json`.
    pub fn base_config(&self) -> PathBuf {
        self.root.join("base/devcontainer.json")
    }

    /// A runtime's config file, `~/.dev/<runtime>/devcontainer.json`.
    pub fn runtime_config(&self, runtime: &str) -> PathBuf {
        self.root.join(runtime).join("devcontainer.json")
    }
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
