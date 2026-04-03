use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use super::jsonc::parse_jsonc;
use crate::error::DevError;
use crate::runtime::PortMapping;

/// A port entry in the `forwardPorts` array: either a plain number or a "host:container" string.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PortEntry {
    Plain(u16),
    Mapping(String),
}

impl PortEntry {
    fn into_port_mapping(self) -> Result<PortMapping, String> {
        match self {
            PortEntry::Plain(p) => Ok(PortMapping {
                host: p,
                container: p,
            }),
            PortEntry::Mapping(s) => {
                let (host_str, container_str) = s
                    .split_once(':')
                    .ok_or_else(|| format!("invalid port mapping: {s}"))?;
                let host: u16 = host_str
                    .parse()
                    .map_err(|_| format!("invalid host port in: {s}"))?;
                let container: u16 = container_str
                    .parse()
                    .map_err(|_| format!("invalid container port in: {s}"))?;
                Ok(PortMapping { host, container })
            }
        }
    }
}

fn deserialize_forward_ports<'de, D>(deserializer: D) -> Result<Option<Vec<PortMapping>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries: Option<Vec<PortEntry>> = Option::deserialize(deserializer)?;
    match entries {
        None => Ok(None),
        Some(v) => {
            let mappings: Result<Vec<PortMapping>, String> =
                v.into_iter().map(|e| e.into_port_mapping()).collect();
            mappings.map(Some).map_err(serde::de::Error::custom)
        }
    }
}

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

/// The `dockerComposeFile` field can be a single path or an array of paths.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DockerComposeFile {
    Single(String),
    Multiple(Vec<String>),
}

impl DockerComposeFile {
    /// Return all compose file paths as a Vec.
    pub fn files(&self) -> Vec<&str> {
        match self {
            DockerComposeFile::Single(s) => vec![s.as_str()],
            DockerComposeFile::Multiple(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// Parsed devcontainer.json configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct DevcontainerConfig {
    pub name: Option<String>,
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    pub docker_compose_file: Option<DockerComposeFile>,
    pub service: Option<String>,
    pub workspace_folder: Option<String>,
    pub features: Option<HashMap<String, serde_json::Value>>,
    #[serde(default, deserialize_with = "deserialize_forward_ports")]
    pub forward_ports: Option<Vec<PortMapping>>,
    pub remote_user: Option<String>,
    pub remote_env: Option<HashMap<String, String>>,
    pub container_env: Option<HashMap<String, String>>,
    pub mounts: Option<Vec<String>>,
    /// Named Docker volumes to mount into the container.
    /// Format: `"volume-name:/container/path"` or `"volume-name:/container/path:ro"`
    pub volumes: Option<Vec<String>>,
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

        // Parse JSONC (supports comments and trailing commas).
        let config: DevcontainerConfig = parse_jsonc(&raw)?;
        Ok(config)
    }

    /// Returns true if this config uses Docker Compose rather than image/Dockerfile.
    pub fn is_compose(&self) -> bool {
        self.docker_compose_file.is_some()
    }
}
