use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::error::DevError;

/// A lifecycle command can be a single string, a list of strings, or named parallel commands.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LifecycleCommand {
    Single(String),
    Multiple(Vec<String>),
    Parallel(HashMap<String, String>),
}

/// Build configuration within devcontainer.json.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BuildConfig {
    pub dockerfile: String,
    pub context: Option<String>,
    pub args: Option<HashMap<String, String>>,
}

/// Parsed devcontainer.json configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct DevcontainerConfig {
    pub name: Option<String>,
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    pub features: Option<HashMap<String, serde_json::Value>>,
    pub forward_ports: Option<Vec<u16>>,
    pub remote_user: Option<String>,
    pub remote_env: Option<HashMap<String, String>>,
    pub container_env: Option<HashMap<String, String>>,
    pub mounts: Option<Vec<String>>,
    pub run_args: Option<Vec<String>>,
    pub on_create_command: Option<LifecycleCommand>,
    pub update_content_command: Option<LifecycleCommand>,
    pub post_create_command: Option<LifecycleCommand>,
    pub post_start_command: Option<LifecycleCommand>,
    pub post_attach_command: Option<LifecycleCommand>,
    pub initialize_command: Option<LifecycleCommand>,
    pub customize: Option<serde_json::Value>,
    /// Whether to update the remote user's UID/GID to match the host user.
    #[serde(rename = "updateRemoteUserUID")]
    pub update_remote_user_uid: Option<bool>,
    /// Dotfiles configuration for cloning a user's dotfiles repo into the container.
    pub dotfiles: Option<DotfilesConfig>,
}

/// Configuration for cloning a dotfiles repository into the container.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct DotfilesConfig {
    /// Git repository URL to clone (e.g. "https://github.com/user/dotfiles.git").
    pub repository: String,
    /// Target path inside the container (default: "~/dotfiles").
    pub target_path: Option<String>,
    /// Command to run after cloning (e.g. "~/dotfiles/install.sh").
    pub install_command: Option<String>,
}

impl DevcontainerConfig {
    /// Read and parse a devcontainer.json file (supports JSONC with comments).
    pub fn from_path(path: &Path) -> Result<Self, DevError> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            DevError::InvalidConfig(format!("Failed to read {}: {e}", path.display()))
        })?;

        // Strip JSONC comments before parsing.
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let config: DevcontainerConfig = serde_json::from_reader(stripped)?;
        Ok(config)
    }
}
