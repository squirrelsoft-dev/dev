use serde::Deserialize;
use std::collections::HashMap;
use std::default::Default;
use std::path::Path;

use super::jsonc::parse_jsonc;
use super::variables::substitute_variables_with_user;
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

/// A mount entry in devcontainer.json's `mounts` array: either a plain Docker-formatted string
/// (short `/host:/container[:ro]` or long `source=X,target=Y,type=bind[,readonly]`)
/// or an object with the mount spec fields. This matches the [devcontainer schema](https://github.com/devcontainers/spec/blob/c95ffeed1d059abfe9ffbe79762dc2fa4e7c2421/schemas/devContainer.base.schema.json).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MountSpec {
    Plain(String),
    Object(MountObject),
}

/// Object-form mount specification fields (all optional per the devcontainer schema).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct MountObject {
    pub source: Option<String>,
    pub target: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub readonly: Option<bool>,
    #[serde(default)]
    pub consistency: Option<String>,
}

impl MountSpec {
    /// Substitute variables in this mount entry and convert to a Docker long-form
    /// string suitable for both the bind-mount parser (`parse_single_mount`) and
    /// the compose override emitter (`mount_to_compose_volume`).
    ///
    /// For string-form mounts this is just variable substitution. For object-form
    /// mounts it substitutes `source`/`target` fields and emits a long-form string.
    /// Returns the substituted string, or `None` if the object lacked `source`/`target`.
    pub fn substitute_and_emit(
        &self,
        workspace: &std::path::Path,
        remote_user: Option<&str>,
    ) -> Option<String> {
        match self {
            MountSpec::Plain(s) => Some(substitute_variables_with_user(s, workspace, remote_user)),
            MountSpec::Object(o) => {
                let src =
                    substitute_variables_with_user(o.source.as_deref()?, workspace, remote_user);
                let tgt =
                    substitute_variables_with_user(o.target.as_deref()?, workspace, remote_user);
                let ty = o.r#type.as_deref().unwrap_or("bind");
                let ro = o.readonly.unwrap_or(false);
                Some(format!("source={src},target={tgt},type={ty},readonly={ro}"))
            }
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
    pub mounts: Option<Vec<MountSpec>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(json: &str) -> DevcontainerConfig {
        parse_jsonc(json).expect("json should parse")
    }

    #[test]
    fn object_mount_deserializes_into_config() {
        let config = parse(
            r#"{
            "name": "test1",
            "image": "alpine:latest",
            "mounts": [
                { "source": "./", "target": "/workspace", "type": "bind" }
            ]
        }"#,
        );
        let mounts = config.mounts.expect("mounts field is present");
        assert_eq!(mounts.len(), 1);
        // Confirm it resolved to MountSpec::Object, not a plain string.
        let src = matches!(&mounts[0], MountSpec::Object(o) if o.source.as_deref() == Some("./"));
        assert!(src, "expected MountSpec::Object with source=./")
    }

    #[test]
    fn string_mount_still_deserializes() {
        let config = parse(
            r#"{
            "name": "test2",
            "image": "alpine:latest",
            "mounts": ["${localEnv:HOME}/.config:/home/vscode/.config:ro"]
        }"#,
        );
        let mounts = config.mounts.expect("mounts field is present");
        assert_eq!(mounts.len(), 1);
        assert!(matches!(&mounts[0], MountSpec::Plain(_)))
    }

    #[test]
    fn mixed_string_and_object_array_deserializes() {
        let config = parse(
            r#"{
            "name": "mixed",
            "image": "alpine:latest",
            "mounts": [
                "${localEnv:HOME}/.config:/home/vscode/.config:ro",
                { "source": "./src", "target": "/workspace/src", "type": "bind" }
            ]
        }"#,
        );
        let mounts = config.mounts.expect("mounts field is present");
        assert_eq!(mounts.len(), 2);
        assert!(matches!(&mounts[0], MountSpec::Plain(_)));
        assert!(matches!(&mounts[1], MountSpec::Object(_)))
    }

    #[test]
    fn object_mount_with_readonly_and_consistency_emits_long_form_string() {
        let ws = PathBuf::from("/home/user/project");
        let spec = MountSpec::Object(MountObject {
            source: Some("./logs".into()),
            target: Some("${localEnv:HOME}/logs".into()),
            r#type: Some("bind".into()),
            readonly: Some(true),
            consistency: Some("fuzzy".into()),
        });
        let emitted = spec
            .substitute_and_emit(&ws, Some("root"))
            .expect("should emit a long-form string");
        // `${localEnv:HOME}` is the host env's HOME, not the container user's.
        assert!(
            emitted.starts_with("source=./logs,target=")
                && emitted.ends_with(",type=bind,readonly=true")
        );
        assert!(emitted.contains("type=bind") && emitted.contains("readonly=true"));
    }

    #[test]
    fn object_mount_without_type_defaults_to_bind() {
        let ws = PathBuf::from("/home/user/project");
        let spec = MountSpec::Object(MountObject {
            source: Some("./".into()),
            target: Some("/workspace".into()),
            r#type: None,
            readonly: Some(false),
            consistency: None,
        });
        let emitted = spec.substitute_and_emit(&ws, None).expect("should emit");
        assert!(emitted.contains("type=bind"));
    }

    #[test]
    fn object_mount_without_source_or_target_emits_none() {
        let ws = PathBuf::from("/home/user/project");
        let spec = MountSpec::Object(MountObject {
            source: None,
            target: Some("/workspace".into()),
            r#type: Some("tmpfs".into()),
            readonly: None,
            consistency: None,
        });
        assert!(spec.substitute_and_emit(&ws, None).is_none());
    }

    #[test]
    fn object_mount_substitutes_container_env_variable() {
        let ws = PathBuf::from("/home/user/project");
        let spec = MountSpec::Object(MountObject {
            source: Some("${containerEnv:HOME}/.config".into()),
            target: Some("${containerEnv:HOME}/.config".into()),
            r#type: Some("bind".into()),
            readonly: Some(false),
            consistency: None,
        });
        let emitted = spec
            .substitute_and_emit(&ws, Some("vscode"))
            .expect("should emit a long-form string");
        // containerEnv:HOME expands to /home/<user> for a non-root user.
        assert!(emitted.contains(
            "source=/home/vscode/.config,target=/home/vscode/.config,type=bind,readonly=false"
        ));
    }

    #[test]
    fn object_mount_with_unknown_extra_fields_emits_long_form_string() {
        // Extra unknown fields should be tolerated by #[serde(untagged)]'s Object branch.
        let ws = PathBuf::from("/home/user/project");
        let spec = MountSpec::Object(MountObject {
            source: Some("./".into()),
            target: Some("/workspace".into()),
            r#type: Some("bind".into()),
            readonly: None,
            consistency: None,
        });
        // The object struct only has known fields; unknown fields are ignored by serde
        // by default, so this parse should be fine end-to-end.
        let config = parse(
            r#"{
            "name": "extras",
            "image": "alpine:latest",
            "mounts": [
                {
                    "source": "./",
                    "target": "/workspace",
                    "type": "bind",
                    "consistency": "fuzzy",
                    "extendedAttribute": true
                }
            ]
        }"#,
        );
        let mounts = config.mounts.expect("mounts field is present");
        assert_eq!(mounts.len(), 1);
        assert!(matches!(&mounts[0], MountSpec::Object(o) if o.source.as_deref() == Some("./")))
    }
}
