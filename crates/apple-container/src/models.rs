use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Snapshot of a container's full state, returned by list/get operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSnapshot {
    pub configuration: ContainerConfiguration,
    pub status: RuntimeStatus,
    #[serde(default)]
    pub networks: Vec<NetworkAttachment>,
    #[serde(default)]
    pub started_date: Option<f64>,
}

/// Full configuration for creating a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfiguration {
    pub id: String,
    #[serde(default)]
    pub image: ImageDescription,
    #[serde(default)]
    pub mounts: Vec<Filesystem>,
    #[serde(default)]
    pub published_ports: Vec<PublishPort>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub init_process: ProcessConfiguration,
    #[serde(default)]
    pub resources: Resources,
}

/// OCI content descriptor (mediaType, digest, size).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciDescriptor {
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub size: u64,
}

/// Describes an OCI image to use for the container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDescription {
    #[serde(default)]
    pub descriptor: OciDescriptor,
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub manifest_digest: String,
}

/// A filesystem mount for the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filesystem {
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub read_only: bool,
}

/// A published port mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishPort {
    pub host_port: u16,
    pub container_port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

/// Process configuration for the init process or exec.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessConfiguration {
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub working_directory: String,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub user: User,
}

/// User identity for a process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

/// Resource limits for the container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    #[serde(default)]
    pub cpu_count: u32,
    #[serde(default)]
    pub memory_in_bytes: u64,
}

/// Runtime status of a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeStatus {
    Unknown,
    Stopped,
    Running,
    Stopping,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Network attachment info.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAttachment {
    #[serde(default)]
    pub network: String,
    #[serde(default, alias = "ipAddress")]
    pub ipv4_address: String,
    #[serde(default)]
    pub ipv6_address: String,
    #[serde(default)]
    pub mac_address: String,
    #[serde(default)]
    pub hostname: String,
}

/// Container statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStats {
    #[serde(default)]
    pub cpu_usage: f64,
    #[serde(default)]
    pub memory_usage: u64,
    #[serde(default)]
    pub disk_usage: u64,
}
