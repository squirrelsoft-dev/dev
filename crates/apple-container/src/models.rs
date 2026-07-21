use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// Container ids already reported as undecodable, so the warning stays a
/// warning instead of becoming noise.
static REPORTED_UNDECODABLE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Whether this container's decode failure has not been reported yet.
///
/// `list` runs many times per command — the `dev up` readiness gate alone polls
/// repeatedly — so an unreadable container would otherwise print the same line
/// a dozen times per invocation.
fn first_report_for(id: &str) -> bool {
    let reported = REPORTED_UNDECODABLE.get_or_init(|| Mutex::new(HashSet::new()));
    let mut reported = reported.lock().unwrap_or_else(|e| e.into_inner());
    reported.insert(id.to_string())
}

/// Decode a `containerList` payload one entry at a time.
///
/// The daemon returns every container it knows about in a single array, so an
/// all-or-nothing decode lets one container the models cannot represent hide
/// all the others — which is how `dev status`/`dev exec`/`dev up` discovery
/// broke in issue #4. Entries that fail to decode are reported on stderr (once
/// per container per process) and skipped so the remaining containers stay
/// discoverable.
pub fn decode_snapshots(data: &[u8]) -> Result<Vec<ContainerSnapshot>, serde_json::Error> {
    let entries: Vec<serde_json::Value> = serde_json::from_slice(data)?;
    let mut snapshots = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = entry
            .get("configuration")
            .and_then(|c| c.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        match serde_json::from_value::<ContainerSnapshot>(entry) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(e) => {
                if first_report_for(&id) {
                    eprintln!(
                        "Warning: ignoring container '{id}': the daemon returned a record this \
                         version cannot read ({e})"
                    );
                }
            }
        }
    }
    Ok(snapshots)
}

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
    /// Name of the runtime plugin to use.  The daemon defaults to
    /// `"container-runtime-linux"` when absent.
    #[serde(default = "default_runtime_handler")]
    pub runtime_handler: String,
    #[serde(default)]
    pub platform: Platform,
    #[serde(default)]
    pub networks: Vec<NetworkInfo>,
    #[serde(default)]
    pub dns: Option<DnsInfo>,
}

fn default_runtime_handler() -> String {
    "container-runtime-linux".to_string()
}

/// OCI content descriptor (mediaType, digest, size, annotations).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciDescriptor {
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,
}

/// Describes an OCI image to use for the container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDescription {
    #[serde(default)]
    pub descriptor: OciDescriptor,
    #[serde(default)]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
}

/// Platform specification for the container (architecture, OS).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    pub architecture: String,
    pub os: String,
}

/// Network attachment configuration for the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    pub network: String,
    pub options: NetworkOptions,
}

/// Network attachment options (hostname, MTU).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkOptions {
    pub hostname: Option<String>,
    pub mtu: Option<u32>,
}

/// DNS configuration for the container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsInfo {
    pub nameservers: Vec<String>,
    pub search_domains: Vec<String>,
    pub options: Vec<String>,
}

/// A filesystem mount for the container.
///
/// The `type` field determines the kind of filesystem attachment.
/// Apple's Codable serializes enum cases as single-key objects, e.g.
/// `{"virtiofs": {}}` or `{"tmpfs": {}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filesystem {
    /// Filesystem type.  Serialized as `"type"` (matching Apple's model).
    #[serde(rename = "type")]
    pub fs_type: FSType,
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub options: Vec<String>,
}

/// Filesystem attachment type, matching Apple's `FSType` enum.
///
/// Swift's Codable encodes each case as `{"<caseName>": {}}` (for empty
/// cases) or `{"<caseName>": {…associated values…}}`.
///
/// `Volume` covers named volumes created by `container volume`/`container
/// run --volume`, which the daemon attaches as a block-backed filesystem.
/// `list`/`inspect` must deserialize these or `dev status`/`dev exec`/`dev up`
/// discovery fails whenever any container in the daemon uses a named volume
/// (issue #4).
///
/// `Other` keeps that class of failure from recurring: any case a newer daemon
/// adds is retained verbatim instead of failing the whole reply. It round-trips
/// unchanged because it is `untagged`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FSType {
    Virtiofs(Empty),
    Tmpfs(Empty),
    Volume(VolumeFilesystem),
    #[serde(untagged)]
    Other(serde_json::Value),
}

/// Empty unit struct used for Swift-compatible enum case encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Empty {}

/// Associated values for the `Volume` case of [`FSType`].
///
/// `cache` and `sync` are themselves Swift enums encoded as single-key
/// objects (e.g. `{"on":{}}`, `{"fsync":{}}`). They are captured opaquely so
/// new modes the daemon adds do not break deserialization of `list`/`inspect`
/// output. Unknown fields are likewise tolerated (serde ignores them by
/// default), so adding fields here is forward-compatible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeFilesystem {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<serde_json::Value>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default = "default_user")]
    pub user: User,
    #[serde(default)]
    pub supplemental_groups: Vec<u32>,
    #[serde(default)]
    pub rlimits: Vec<Rlimit>,
}

fn default_user() -> User {
    User::Id {
        id: UserId { uid: 0, gid: 0 },
    }
}

impl Default for ProcessConfiguration {
    fn default() -> Self {
        Self {
            executable: String::new(),
            arguments: vec![],
            environment: vec![],
            working_directory: String::new(),
            terminal: false,
            user: default_user(),
            supplemental_groups: vec![],
            rlimits: vec![],
        }
    }
}

/// User identity for a process.
///
/// Apple's daemon uses an enum with two cases.  Swift's Codable preserves
/// associated-value labels as inner keys:
/// - `.id(uid:gid:)` → `{"id": {"uid": 0, "gid": 0}}`
/// - `.raw(userString:)` → `{"raw": {"userString": "root"}}`
///
/// `#[serde(untagged)]` matches Swift's output (no outer case-name tag).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum User {
    Id { id: UserId },
    Raw { raw: UserString },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserId {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserString {
    pub user_string: String,
}

/// Resource limits for a process (e.g. RLIMIT_NOFILE).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rlimit {
    pub limit: String,
    pub soft: u64,
    pub hard: u64,
}

/// Resource limits for the container.
///
/// Apple's daemon enforces a minimum of 200 MiB memory.  The defaults
/// match the daemon's own `Resources` struct (4 CPUs, 1 GiB memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    /// Number of CPU cores.  Serialized as `"cpus"` (matching Apple's key).
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    /// Memory in bytes.  Serialized as `"memoryInBytes"`.
    #[serde(default = "default_memory")]
    pub memory_in_bytes: u64,
}

fn default_cpus() -> u32 {
    4
}

fn default_memory() -> u64 {
    1024 * 1024 * 1024 // 1 GiB
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            cpus: default_cpus(),
            memory_in_bytes: default_memory(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `list`/`inspect` reply must deserialize even when a container in the
    /// daemon uses a named volume filesystem. Before the `Volume` variant was
    /// added, `FSType` rejected `"volume"` and `container ls`/`container inspect`
    /// — and therefore `dev status`/`dev exec`/`dev up` discovery — failed for
    /// every container whenever any one container had a volume mount (issue #4).
    #[test]
    fn filesystem_with_volume_mount_deserializes() {
        let json = r#"{
            "type": {"volume": {"name": "move-pg-data", "cache": {"on": {}}, "sync": {"fsync": {}}, "format": "ext4"}},
            "source": "/Users/u/Library/Application Support/com.apple.container/volumes/move-pg-data/volume.img",
            "destination": "/var/lib/postgresql/data",
            "options": []
        }"#;
        let fs: Filesystem =
            serde_json::from_str(json).expect("volume filesystem must deserialize");
        match fs.fs_type {
            FSType::Volume(v) => {
                assert_eq!(v.name, "move-pg-data");
                assert_eq!(v.format, "ext4");
            }
            other => panic!("expected Volume, got {other:?}"),
        }
    }

    /// A complete `containerList` snapshot containing a volume mount and the
    /// extra configuration fields the daemon emits (`rosetta`, `ssh`,
    /// `useInit`, `publishedSockets`, `sysctls`, `readOnly`, `virtualization`)
    /// must deserialize. This is the shape that broke `dev status` on a daemon
    /// that also hosted a postgres container with a named volume.
    #[test]
    fn container_snapshot_with_volume_and_extra_fields_deserializes() {
        let json = r#"{
            "configuration": {
                "id": "move-pg",
                "image": {"reference": "docker.io/library/postgres:16"},
                "mounts": [{
                    "type": {"volume": {"name": "move-pg-data", "cache": {"on": {}}, "sync": {"fsync": {}}, "format": "ext4"}},
                    "source": "/v.img",
                    "destination": "/data",
                    "options": []
                }],
                "publishedPorts": [],
                "labels": {},
                "initProcess": {"executable": "postgres", "arguments": [], "environment": [], "workingDirectory": "/", "terminal": false},
                "resources": {"cpus": 4, "memoryInBytes": 1073741824},
                "runtimeHandler": "container-runtime-linux",
                "platform": {"architecture": "arm64", "os": "linux"},
                "networks": [{"network": "default", "options": {"hostname": "move-pg"}}],
                "rosetta": false,
                "virtualization": false,
                "readOnly": false,
                "useInit": false,
                "publishedSockets": [],
                "ssh": false,
                "sysctls": {}
            },
            "status": "running",
            "networks": [],
            "startedDate": 806283546.523321
        }"#;
        let snap: ContainerSnapshot =
            serde_json::from_str(json).expect("snapshot with volume mount must deserialize");
        assert_eq!(snap.configuration.id, "move-pg");
        assert!(matches!(
            snap.configuration.mounts[0].fs_type,
            FSType::Volume(_)
        ));
        assert_eq!(snap.status, RuntimeStatus::Running);
    }

    /// A mount case this version does not model must still deserialize, and must
    /// survive a round-trip unchanged — otherwise the next filesystem type the
    /// daemon adds reproduces the issue #4 discovery break.
    #[test]
    fn filesystem_with_unknown_type_falls_back_to_other() {
        let json = r#"{
            "type": {"blockDevice": {"path": "/dev/disk4", "readOnly": true}},
            "source": "/dev/disk4",
            "destination": "/mnt/data",
            "options": []
        }"#;
        let fs: Filesystem =
            serde_json::from_str(json).expect("unknown filesystem type must deserialize");
        match &fs.fs_type {
            FSType::Other(value) => assert_eq!(
                value.get("blockDevice").and_then(|b| b.get("path")),
                Some(&serde_json::Value::from("/dev/disk4"))
            ),
            other => panic!("expected Other, got {other:?}"),
        }

        let reencoded = serde_json::to_value(&fs).expect("re-encode");
        assert_eq!(
            reencoded.get("type"),
            serde_json::from_str::<serde_json::Value>(json)
                .unwrap()
                .get("type"),
            "an unmodelled mount type must round-trip verbatim"
        );
    }

    fn snapshot_json(id: &str, mount_type: &str) -> String {
        format!(
            r#"{{
                "configuration": {{
                    "id": "{id}",
                    "image": {{"reference": "docker.io/library/alpine:latest"}},
                    "mounts": [{{
                        "type": {mount_type},
                        "source": "/src",
                        "destination": "/dst",
                        "options": []
                    }}],
                    "labels": {{"devcontainer.local_folder": "/w/{id}"}}
                }},
                "status": "running"
            }}"#
        )
    }

    /// One container the models cannot represent must not hide the rest of the
    /// list. This is the issue #4 failure mode: an unrelated container in the
    /// daemon made `dev status`/`dev exec`/`dev up` discovery fail for *every*
    /// workspace, because `containerList` was decoded all-or-nothing.
    #[test]
    fn decode_snapshots_skips_unreadable_entries() {
        // `status` is required, so an entry missing it cannot be decoded at all
        // — it stands in for whatever the next unmodelled daemon field is.
        let payload = format!(
            "[{},{},{}]",
            snapshot_json("good-one", r#"{"virtiofs": {}}"#),
            r#"{"configuration": {"id": "unreadable"}}"#,
            snapshot_json("good-two", r#"{"volume": {"name": "v", "format": "ext4"}}"#),
        );

        let snapshots = decode_snapshots(payload.as_bytes()).expect("list payload must decode");

        let ids: Vec<&str> = snapshots
            .iter()
            .map(|s| s.configuration.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["good-one", "good-two"],
            "readable containers must stay discoverable when a sibling entry cannot be decoded"
        );
    }

    /// The undecodable-container warning is reported once per container, not
    /// once per `list` call — `dev up` alone polls the list a dozen times, and
    /// a repeated line buries the signal it is meant to carry.
    #[test]
    fn undecodable_container_is_reported_once() {
        let payload = br#"[{"configuration": {"id": "noisy-container"}}]"#;

        for round in 0..5 {
            let snapshots = decode_snapshots(payload).expect("list payload must decode");
            assert!(snapshots.is_empty());
            assert!(
                !first_report_for("noisy-container"),
                "round {round} must not re-report an already reported container"
            );
        }
    }

    /// A payload that is not a JSON array at all is still a hard error — the
    /// per-entry tolerance must not turn a malformed reply into "no containers".
    #[test]
    fn decode_snapshots_rejects_non_array_payload() {
        decode_snapshots(b"{\"containers\": []}")
            .expect_err("a non-array containerList payload must surface as an error");
    }
}
