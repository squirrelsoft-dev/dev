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

/// Runtime-specific config directory: `~/.dev/<name>/`
pub fn runtime_config_dir(name: &str) -> PathBuf {
    dev_home().join(name)
}

/// VS Code remote-containers configs directory.
/// `~/Library/Application Support/Code/User/globalStorage/ms-vscode-remote.remote-containers/configs/`
pub fn vscode_configs_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join("Library/Application Support/Code/User/globalStorage/ms-vscode-remote.remote-containers/configs")
}
