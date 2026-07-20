use std::path::PathBuf;

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

    /// The global templates directory, `~/.dev/global/`.
    pub fn global_dir(&self) -> PathBuf {
        self.root.join("global")
    }

    /// The user-scoped project devcontainers directory, `~/.dev/devcontainers/`.
    pub fn devcontainers_dir(&self) -> PathBuf {
        self.root.join("devcontainers")
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
