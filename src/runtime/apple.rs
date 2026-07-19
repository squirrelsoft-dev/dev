use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::Path;

use apple_container::models::{
    ContainerConfiguration, Empty, FSType, Filesystem, ImageDescription, ProcessConfiguration,
    PublishPort, Resources, RuntimeStatus, User, UserId, UserString,
};
use apple_container::AppleContainerClient;

use crate::error::DevError;
use crate::runtime::{
    AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
    ExecResult, ImageMetadata,
};

/// Apple Containers runtime using native XPC.
pub struct AppleRuntime {
    client: AppleContainerClient,
}

impl AppleRuntime {
    pub fn connect() -> Result<Self, DevError> {
        let client = AppleContainerClient::connect()
            .map_err(|e| DevError::Runtime(format!("Apple Containers: {e}")))?;
        Ok(Self { client })
    }

    pub async fn ping(&self) -> Result<(), DevError> {
        self.client
            .ping()
            .await
            .map_err(|e| DevError::Runtime(format!("Apple Containers ping failed: {e}")))?;
        Ok(())
    }

    /// Search the Apple Container local image store for a reference.
    ///
    /// Returns the raw ImageDescription bytes if found, or None if the image
    /// is not present locally and must be pulled from a registry.
    async fn find_local_image(&self, reference: &str) -> Option<Vec<u8>> {
        match self.client.image_list().await {
            Ok(images) => {
                for img in &images {
                    if img.reference == reference {
                        return serde_json::to_vec(&img).ok();
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: could not list local images: {e}");
            }
        }
        None
    }
}

/// Return the path to the OCI config cache directory.
fn oci_config_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("devcontainer")
        .join("oci-configs")
}

/// Cache key for an image reference (SHA-256 of normalized ref).
fn cache_key(reference: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(reference.as_bytes());
    hex::encode(hasher.finalize())
}

/// Cached OCI image config fields we care about.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedImageConfig {
    env: Vec<String>,
    user: Option<String>,
    working_dir: Option<String>,
}

/// Read cached OCI image config for a reference, if present.
fn read_cached_config(reference: &str) -> Option<CachedImageConfig> {
    let path = oci_config_cache_dir().join(cache_key(reference)).with_extension("json");
    let data = std::fs::read(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Write cached OCI image config for a reference.
fn write_cached_config(reference: &str, config: &CachedImageConfig) -> Result<(), DevError> {
    let dir = oci_config_cache_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DevError::Runtime(format!("create cache dir: {e}")))?;
    let path = dir.join(cache_key(reference)).with_extension("json");
    let data = serde_json::to_vec(config)
        .map_err(|e| DevError::Runtime(format!("serialize cache: {e}")))?;
    std::fs::write(&path, &data)
        .map_err(|e| DevError::Runtime(format!("write cache: {e}")))?;
    Ok(())
}

/// Platform resolver that chooses the first linux/arm64 variant from an image index.
fn linux_arm64_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().is_some_and(|platform| {
                platform.os == "linux" && platform.architecture == "arm64"
            })
        })
        .map(|entry| entry.digest.clone())
}

/// Fetch the OCI image config from a registry and cache it locally.
///
/// Uses `oci_client` to authenticate, pull the manifest, then pull the
/// config blob described by `manifest.config.digest`.  The parsed
/// `env`, `user`, and `working_dir` fields are stored in
/// `~/.cache/devcontainer/oci-configs/` for later use by
/// `to_apple_config`.
async fn fetch_and_cache_oci_config(reference: &str) -> Result<CachedImageConfig, DevError> {
    let oci_ref: oci_client::Reference = reference.parse().map_err(|e: oci_client::ParseError| {
        DevError::Runtime(format!("invalid image ref: {e}"))
    })?;

    let client = oci_client::Client::new(oci_client::client::ClientConfig {
        platform_resolver: Some(Box::new(linux_arm64_resolver)),
        ..Default::default()
    });
    let auth = oci_client::secrets::RegistryAuth::Anonymous;
    client
        .auth(&oci_ref, &auth, oci_client::RegistryOperation::Pull)
        .await
        .map_err(|e| DevError::Runtime(format!("registry auth failed: {e}")))?;

    let (manifest, _digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| DevError::Runtime(format!("pull manifest for {reference}: {e}")))?;

    let mut config_data = Vec::new();
    client
        .pull_blob(
            &oci_ref,
            manifest.config.digest.as_str(),
            &mut config_data,
        )
        .await
        .map_err(|e| DevError::Runtime(format!("pull config blob for {reference}: {e}")))?;

    let config_json: serde_json::Value = serde_json::from_slice(&config_data)
        .map_err(|e| DevError::Runtime(format!("parse image config JSON: {e}")))?;

    let env = config_json
        .get("config")
        .and_then(|c| c.get("Env"))
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let user = config_json
        .get("config")
        .and_then(|c| c.get("User"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());

    let working_dir = config_json
        .get("config")
        .and_then(|c| c.get("WorkingDir"))
        .and_then(|w| w.as_str())
        .map(|s| s.to_string());

    let cached = CachedImageConfig {
        env,
        user,
        working_dir,
    };
    if let Err(e) = write_cached_config(reference, &cached) {
        eprintln!("Warning: failed to cache OCI config: {e}");
    }
    Ok(cached)
}

/// RAII guard that puts the terminal into raw mode and restores it on drop.
struct RawModeGuard {
    original: libc::termios,
    fd: i32,
}

impl RawModeGuard {
    fn enter() -> Result<Self, DevError> {
        let fd = std::io::stdin().as_raw_fd();
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(DevError::Runtime("Failed to get terminal attributes".into()));
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(DevError::Runtime("Failed to set raw mode".into()));
        }
        Ok(Self { original, fd })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

/// Translate Shift+Enter escape sequences into a plain carriage return.
#[allow(dead_code)]
fn translate_shift_enter(input: &[u8]) -> Vec<u8> {
    const CSI_U: &[u8] = b"\x1b[13;2u";
    const XTERM: &[u8] = b"\x1b[27;2;13~";

    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            if input[i..].starts_with(CSI_U) {
                out.push(b'\r');
                i += CSI_U.len();
                continue;
            }
            if input[i..].starts_with(XTERM) {
                out.push(b'\r');
                i += XTERM.len();
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// Convert our generic ContainerConfig to the Apple Containers configuration.
/// Merge environment variables from the OCI image config with the container
/// config env, matching Apple `container` CLI's `Parser.allEnv()` order:
/// image env → container config env → runtime env (later overrides earlier).
fn merge_env(image_env: &[String], container_env: &HashMap<String, String>) -> Vec<String> {
    let mut merged: HashMap<String, String> = HashMap::new();

    // 1. Image env as base.
    for entry in image_env {
        if let Some((k, v)) = entry.split_once('=') {
            merged.insert(k.to_string(), v.to_string());
        }
    }

    // 2. Container config env overrides image env.
    for (k, v) in container_env {
        merged.insert(k.clone(), v.clone());
    }

    merged.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Apple's `vmexec` rejects container IDs longer than this (UUID length).
const MAX_CONTAINER_ID_LEN: usize = 36;

/// Shorten a container name to fit Apple's ID length limit.
///
/// Keeps the leading prefix for human readability and the trailing bytes for
/// uniqueness. `container_name()` emits `vsc-<dir>-<64-hex sha of abs path>`, so the
/// retained suffix is 18 hex chars of that hash — enough entropy that two workspaces
/// sharing a truncated prefix still get distinct IDs.
fn truncate_container_id(name: &str) -> String {
    if name.len() <= MAX_CONTAINER_ID_LEN {
        return name.to_string();
    }
    // 17 + '-' + 18 == MAX_CONTAINER_ID_LEN.
    format!("{}-{}", &name[..17], &name[name.len() - 18..])
}

fn to_apple_config(
    config: &ContainerConfig,
    image: ImageDescription,
    image_env: &[String],
) -> ContainerConfiguration {
    let mounts: Vec<Filesystem> = config
        .mounts
        .iter()
        .map(|m| Filesystem {
                fs_type: FSType::Virtiofs(Empty {}),
            source: m.source.display().to_string(),
            destination: m.target.clone(),
            options: if m.readonly { vec!["ro".to_string()] } else { vec![] },
        })
        .chain(config.workspace_mount.iter().map(|ws| Filesystem {
                fs_type: FSType::Virtiofs(Empty {}),
            source: ws.source.display().to_string(),
            destination: ws.target.clone(),
            options: vec![],
        }))
        .collect();

    let published_ports: Vec<PublishPort> = config
        .ports
        .iter()
        .map(|p| PublishPort {
            host_port: p.host,
            container_port: p.container,
            protocol: "tcp".to_string(),
        })
        .collect();

    let init_env = merge_env(image_env, &config.env);

    let init_process = ProcessConfiguration {
        executable: config
            .entrypoint
            .clone()
            .unwrap_or_else(|| "sleep".to_string()),
        arguments: if config.entrypoint.is_none() {
            vec!["infinity".to_string()]
        } else {
            Vec::new()
        },
        environment: init_env,
        working_directory: "/".to_string(),
        terminal: false,
        user: User::Raw {
            raw: UserString {
                user_string: "root".to_string(),
            },
        },
        supplemental_groups: vec![],
        rlimits: vec![],
    };

    let id = truncate_container_id(&config.name);
    let hostname = id.clone();

    ContainerConfiguration {
        id,
        image,
        mounts,
        published_ports,
        labels: config.labels.clone(),
        init_process,
        resources: Resources {
            cpus: 4,
            memory_in_bytes: 1024 * 1024 * 1024, // 1 GiB
        },
        runtime_handler: "container-runtime-linux".to_string(),
        platform: apple_container::models::Platform {
            architecture: "arm64".to_string(),
            os: "linux".to_string(),
        },
        networks: vec![apple_container::models::NetworkInfo {
            network: "default".to_string(),
            options: apple_container::models::NetworkOptions {
                hostname: Some(hostname),
                mtu: Some(1280),
            },
        }],
        dns: Some(apple_container::models::DnsInfo {
            nameservers: vec![],
            search_domains: vec![],
            options: vec![],
        }),
    }
}

impl ContainerRuntime for AppleRuntime {
    fn runtime_name(&self) -> &'static str {
        "apple"
    }

    fn pull_image(&self, _image: &str) -> BoxFut<'_, ()> {
        // Pulled inline in create_container so we can use the real descriptor.
        Box::pin(async { Ok(()) })
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        _build_args: &std::collections::HashMap<String, String>,
        no_cache: bool,
        verbose: bool,
    ) -> BoxFut<'_, ()> {
        let dockerfile = dockerfile.to_string();
        let context = context.to_path_buf();
        let tag = tag.to_string();
        Box::pin(async move {
            self.client
                .build(&dockerfile, &context, &tag, no_cache, verbose)
                .await
                .map_err(|e| DevError::BuildFailed(format!("{e}")))
        })
    }

    fn create_container(&self, config: &ContainerConfig) -> BoxFut<'_, String> {
        let config = config.clone();
        Box::pin(async move {
            let platform = serde_json::json!({
                "architecture": "arm64",
                "os": "linux"
            });
            let platform_json = serde_json::to_vec(&platform)
                .map_err(|e| DevError::Runtime(format!("serialize platform: {e}")))?;

            // Try to find the image in the local Apple Container store first.
            // Apple's image service treats all references as remote registry URLs,
            // so `localhost/foo:latest` gets parsed as hostname `localhost`.
            // Local images must be resolved from the image list before pulling.
            let image_desc_bytes = match self.find_local_image(&config.image).await {
                Some(desc) => desc,
                None => {
                    apple_container::build::pull_image(&config.image, &platform_json)
                        .await
                        .map_err(|e| DevError::Runtime(format!("pull image: {e}")))?
                }
            };

            apple_container::build::unpack_image(&image_desc_bytes, &platform_json)
                .await
                .map_err(|e| DevError::Runtime(format!("unpack image: {e}")))?;
            let image: ImageDescription = serde_json::from_slice(&image_desc_bytes)
                .map_err(|e| DevError::Runtime(format!("parse image descriptor: {e}")))?;

            // Fetch OCI image config env vars (cached or from registry) so the
            // init process inherits the image's expected environment.
            let cached = read_cached_config(&config.image);
            let image_env = if let Some(c) = cached {
                c.env
            } else if config.image.starts_with("localhost/") {
                // Local images have no registry to query; daemon handles env vars.
                Vec::new()
            } else {
                match fetch_and_cache_oci_config(&config.image).await {
                    Ok(c) => c.env,
                    Err(e) => {
                        eprintln!(
                            "Warning: could not fetch image config for {}: {e}",
                            config.image
                        );
                        Vec::new()
                    }
                }
            };

            let apple_config = to_apple_config(&config, image, &image_env);
            let id = apple_config.id.clone();

            // Report truncation here rather than in the caller: only this runtime
            // shortens IDs, and the Docker/Podman runtimes return a daemon-assigned
            // ID that never equals the requested name.
            if id != config.name {
                eprintln!(
                    "Container ID truncated to '{id}' to fit Apple's \
                     {MAX_CONTAINER_ID_LEN}-character limit."
                );
            }

            let kernel = self.client
                .get_default_kernel()
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to get default kernel: {e}")))?;

            self.client
                .create(&apple_config, &kernel)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to create container: {e}")))?;

            Ok(id)
        })
    }

    fn start_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move {
            let devnull = std::fs::File::open("/dev/null")
                .map_err(|e| DevError::Runtime(format!("Failed to open /dev/null: {e}")))?;
            let fd = devnull.as_raw_fd();
            self.client
                .bootstrap(&id, fd, fd, fd)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to bootstrap container: {e}")))?;

            self.client
                .start_process(&id, &id)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to start container: {e}")))?;

            Ok(())
        })
    }

    fn stop_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move {
            self.client
                .stop(&id)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to stop container: {e}")))
        })
    }

    fn remove_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move {
            self.client
                .delete(&id, true)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to remove container: {e}")))
        })
    }

    fn exec(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ExecResult> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move {
            let (stdin_read, _stdin_write) =
                os_pipe::pipe().map_err(|e| DevError::Runtime(format!("pipe: {e}")))?;
            let (stdout_read, stdout_write) =
                os_pipe::pipe().map_err(|e| DevError::Runtime(format!("pipe: {e}")))?;
            let (stderr_read, stderr_write) =
                os_pipe::pipe().map_err(|e| DevError::Runtime(format!("pipe: {e}")))?;

            let executable = cmd.first().cloned().unwrap_or_default();
            let arguments = if cmd.len() > 1 { cmd[1..].to_vec() } else { Vec::new() };

            let process_id = format!("exec-{}", std::process::id());

            let uid = user
                .as_deref()
                .and_then(|u| u.parse::<u32>().ok())
                .unwrap_or(0);

            let proc_config = ProcessConfiguration {
                executable,
                arguments,
                environment: Vec::new(),
                working_directory: "/".to_string(),
                terminal: false,
                user: User::Id {
                    id: UserId { uid, gid: uid },
                },
                supplemental_groups: vec![],
                rlimits: vec![],
            };

            self.client
                .create_process(
                    &id,
                    &process_id,
                    &proc_config,
                    stdin_read.as_raw_fd(),
                    stdout_write.as_raw_fd(),
                    stderr_write.as_raw_fd(),
                )
                .await
                .map_err(|e| DevError::Runtime(format!("exec failed: {e}")))?;

            // Close write ends so reads will see EOF.
            drop(stdout_write);
            drop(stderr_write);

            // Read stdout and stderr.
            use std::io::Read;
            let mut stdout_buf = String::new();
            let mut stderr_buf = String::new();

            let mut stdout_read = stdout_read;
            let mut stderr_read = stderr_read;

            stdout_read
                .read_to_string(&mut stdout_buf)
                .map_err(|e| DevError::Runtime(format!("read stdout: {e}")))?;
            stderr_read
                .read_to_string(&mut stderr_buf)
                .map_err(|e| DevError::Runtime(format!("read stderr: {e}")))?;

            Ok(ExecResult {
                exit_code: 0,
                stdout: stdout_buf,
                stderr: stderr_buf,
            })
        })
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ()> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move {
            let _raw_guard = RawModeGuard::enter()?;

            let executable = cmd.first().cloned().unwrap_or_default();
            let arguments = if cmd.len() > 1 { cmd[1..].to_vec() } else { Vec::new() };

            let process_id = format!("exec-interactive-{}", std::process::id());

            let uid = user
                .as_deref()
                .and_then(|u| u.parse::<u32>().ok())
                .unwrap_or(0);

            let proc_config = ProcessConfiguration {
                executable,
                arguments,
                environment: Vec::new(),
                working_directory: "/".to_string(),
                terminal: true,
                user: User::Id {
                    id: UserId { uid, gid: uid },
                },
                supplemental_groups: vec![],
                rlimits: vec![],
            };

            // For interactive mode, pass the real terminal fds directly.
            let stdin_fd = std::io::stdin().as_raw_fd();
            let stdout_fd = std::io::stdout().as_raw_fd();
            let stderr_fd = std::io::stderr().as_raw_fd();

            self.client
                .create_process(
                    &id,
                    &process_id,
                    &proc_config,
                    stdin_fd,
                    stdout_fd,
                    stderr_fd,
                )
                .await
                .map_err(|e| DevError::Runtime(format!("exec_interactive failed: {e}")))?;

            // _raw_guard is dropped here, restoring the terminal.
            Ok(())
        })
    }

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo> {
        let id = id.to_string();
        Box::pin(async move {
            let snapshot = self
                .client
                .get(&id)
                .await
                .map_err(|e| DevError::Runtime(format!("inspect failed: {e}")))?;

            let state = match snapshot.status {
                RuntimeStatus::Running => ContainerState::Running,
                _ => ContainerState::Stopped,
            };

            Ok(ContainerInfo {
                id: snapshot.configuration.id.clone(),
                name: snapshot.configuration.id.clone(),
                state,
                labels: snapshot.configuration.labels.clone(),
                image: snapshot.configuration.image.reference.clone(),
            })
        })
    }

    fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
        let label_filters = label_filters.to_vec();
        Box::pin(async move {
            let snapshots = self
                .client
                .list()
                .await
                .map_err(|e| DevError::Runtime(format!("list failed: {e}")))?;

            // Parse each "key=value" filter and require all to match (AND semantics).
            let parsed_filters: Vec<(&str, &str)> = label_filters
                .iter()
                .map(|f| f.split_once('=').unwrap_or((f.as_str(), "")))
                .collect();

            let mut result = Vec::new();
            for snap in snapshots {
                let all_match = parsed_filters.iter().all(|(key, value)| {
                    snap.configuration
                        .labels
                        .get(*key)
                        .is_some_and(|v| value.is_empty() || v == value)
                });

                if all_match {
                    let state = match snap.status {
                        RuntimeStatus::Running => ContainerState::Running,
                        _ => ContainerState::Stopped,
                    };

                    result.push(ContainerInfo {
                        id: snap.configuration.id.clone(),
                        name: snap.configuration.id.clone(),
                        state,
                        labels: snap.configuration.labels.clone(),
                        image: snap.configuration.image.reference.clone(),
                    });
                }
            }

            Ok(result)
        })
    }

    fn image_exists(&self, _image: &str) -> BoxFut<'_, bool> {
        // Apple Containers doesn't have a local image store to query;
        // always return false so the build path runs.
        Box::pin(async { Ok(false) })
    }

    fn inspect_image_metadata(&self, image: &str) -> BoxFut<'_, ImageMetadata> {
        let image = image.to_string();
        Box::pin(async move {
            // Try to read cached OCI config for this image.
            let cached = read_cached_config(&image);
            if let Some(c) = cached {
                return Ok(ImageMetadata {
                    env: c.env,
                    container_user: c.user,
                    ..ImageMetadata::default()
                });
            }

            // Local images have no registry to query.
            if image.starts_with("localhost/") {
                return Ok(ImageMetadata::default());
            }

            // Not cached — attempt to fetch from registry.
            match fetch_and_cache_oci_config(&image).await {
                Ok(c) => Ok(ImageMetadata {
                    env: c.env.clone(),
                    container_user: c.user,
                    ..ImageMetadata::default()
                }),
                Err(_) => Ok(ImageMetadata::default()),
            }
        })
    }

    fn exec_attached(
        &self,
        _id: &str,
        _cmd: &[String],
        _user: Option<&str>,
    ) -> BoxFut<'_, AttachedExec> {
        Box::pin(async {
            Err(DevError::Runtime(
                "Port forwarding is not yet supported for Apple Containers".into(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Non-test code refers to these via fully-qualified paths, so they are not in
    // the module-level import that `use super::*` re-exports.
    use apple_container::models::{DnsInfo, NetworkInfo, NetworkOptions, Platform};
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// A name at or under the limit must pass through untouched.
    #[test]
    fn short_container_id_is_not_truncated() {
        let short = "vsc-tiny-project";
        assert_eq!(truncate_container_id(short), short);

        let exact = "a".repeat(MAX_CONTAINER_ID_LEN);
        assert_eq!(truncate_container_id(&exact), exact);
    }

    /// Truncation must land exactly on the limit — one byte over and the daemon
    /// rejects the create with EINVAL.
    #[test]
    fn truncated_container_id_has_exact_max_length() {
        let name = format!("vsc-my-project-{}", "a".repeat(64));
        let id = truncate_container_id(&name);
        assert_eq!(id.len(), MAX_CONTAINER_ID_LEN);
    }

    /// Two workspaces whose names share a long prefix must not collide. The retained
    /// suffix is what supplies the entropy, so a prefix-only truncation would be wrong.
    #[test]
    fn truncation_preserves_distinguishing_suffix() {
        let a = format!("vsc-my-project-with-a-long-name-{}", "1".repeat(64));
        let b = format!("vsc-my-project-with-a-long-name-{}", "2".repeat(64));

        let (id_a, id_b) = (truncate_container_id(&a), truncate_container_id(&b));

        assert_ne!(id_a, id_b, "names differing only in suffix must not collide");
        assert!(id_a.ends_with(&a[a.len() - 18..]));
    }

    /// Direct integration test: create + start a container using AppleRuntime,
    /// mirroring the minimal test from apple-test.
    ///
    /// Ignored by default: requires a running Apple Container daemon and pulls an
    /// image over the network. Run with `cargo test --features apple -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a live Apple Container daemon and network access"]
    #[cfg(target_os = "macos")]
    async fn test_apple_runtime_lifecycle() {
        let runtime = AppleRuntime::connect().expect("connect failed");
        let container_id = "apple-runtime-test-lifecycle";
        let image_ref = "mcr.microsoft.com/devcontainers/base:ubuntu";

        // Pull and unpack image
        let platform = serde_json::json!({"architecture": "arm64", "os": "linux"});
        let platform_json = serde_json::to_vec(&platform).unwrap();
        let image_desc_bytes = apple_container::build::pull_image(image_ref, &platform_json)
            .await
            .expect("pull_image failed");
        apple_container::build::unpack_image(&image_desc_bytes, &platform_json)
            .await
            .expect("unpack_image failed");
        let image: ImageDescription = serde_json::from_slice(&image_desc_bytes)
            .expect("parse image descriptor failed");

        // Get default kernel
        let kernel = runtime.client
            .get_default_kernel()
            .await
            .expect("get_default_kernel failed");

        // Build config matching the minimal test
        let config = ContainerConfiguration {
            id: container_id.to_string(),
            image,
            mounts: vec![],
            published_ports: vec![],
            labels: HashMap::new(),
            init_process: ProcessConfiguration {
                executable: "sleep".to_string(),
                arguments: vec!["3600".to_string()],
                environment: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()],
                working_directory: "/".to_string(),
                terminal: false,
                user: User::Raw {
                    raw: UserString { user_string: "root".to_string() },
                },
                supplemental_groups: vec![],
                rlimits: vec![],
            },
            resources: Resources { cpus: 4, memory_in_bytes: 1024 * 1024 * 1024 },
            runtime_handler: "container-runtime-linux".to_string(),
            platform: Platform { architecture: "arm64".to_string(), os: "linux".to_string() },
            networks: vec![NetworkInfo {
                network: "default".to_string(),
                options: NetworkOptions { hostname: Some(container_id.to_string()), mtu: Some(1280) },
            }],
            dns: Some(DnsInfo { nameservers: vec![], search_domains: vec![], options: vec![] }),
        };

        // Clean up any previous test container
        let _ = runtime.client.stop(container_id).await;
        let _ = runtime.client.delete(container_id, true).await;

        // Create
        runtime.client.create(&config, &kernel).await.expect("create failed");

        // Start (bootstrap + start_process)
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = devnull.as_raw_fd();
        runtime.client.bootstrap(container_id, fd, fd, fd).await.expect("bootstrap failed");
        runtime.client.start_process(container_id, container_id).await.expect("start_process failed");

        // Verify running
        let snapshot = runtime.client.get(container_id).await.expect("get failed");
        assert_eq!(snapshot.status, RuntimeStatus::Running, "container should be running");

        // Clean up
        runtime.client.stop(container_id).await.expect("stop failed");
        runtime.client.delete(container_id, true).await.expect("delete failed");
    }

    /// Integration test using the public runtime API (create_container / start_container)
    /// with a realistic ContainerConfig matching what `dev up` produces.
    ///
    /// Ignored by default: requires a running Apple Container daemon and pulls an
    /// image over the network. Run with `cargo test --features apple -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a live Apple Container daemon and network access"]
    #[cfg(target_os = "macos")]
    async fn test_apple_runtime_api_lifecycle() {
        let runtime = AppleRuntime::connect().expect("connect failed");
        // Use a long ID (>36 chars) to verify truncation works
        let container_id = "vsc-test-apple-workspace-d3a8ce6bf5e568384dcfdf4b671042dd9e069a6645ad70d422ff0f4f8f793b62";
        let truncated_id = format!("{}-{}", &container_id[..17], &container_id[container_id.len()-18..]);
        let image_ref = "mcr.microsoft.com/devcontainers/base:ubuntu";

        // Pull and unpack image (same as dev up)
        let platform = serde_json::json!({"architecture": "arm64", "os": "linux"});
        let platform_json = serde_json::to_vec(&platform).unwrap();
        let image_desc_bytes = apple_container::build::pull_image(image_ref, &platform_json)
            .await
            .expect("pull_image failed");
        apple_container::build::unpack_image(&image_desc_bytes, &platform_json)
            .await
            .expect("unpack_image failed");

        // Create a temp workspace directory following the project's test conventions
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path().to_path_buf();
        let devcontainer_dir = workspace_path.join(".devcontainer");
        std::fs::create_dir_all(&devcontainer_dir).unwrap();
        std::fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"name":"Test","image":"mcr.microsoft.com/devcontainers/base:ubuntu"}"#
        ).unwrap();

        // Build a ContainerConfig matching what dev up would create
        let container_config = ContainerConfig {
            image: image_ref.to_string(),
            name: container_id.to_string(),
            labels: {
                let mut labels = HashMap::new();
                labels.insert("devcontainer.local_folder".to_string(), workspace_path.to_string_lossy().to_string());
                labels.insert("devcontainer.config_file".to_string(), devcontainer_dir.join("devcontainer.json").to_string_lossy().to_string());
                labels
            },
            env: {
                let mut env = HashMap::new();
                env.insert("REMOTE_CONTAINERS".to_string(), "true".to_string());
                env
            },
            mounts: vec![],
            volumes: vec![],
            ports: vec![],
            workspace_mount: Some(crate::runtime::WorkspaceMount {
                source: workspace_path.clone(),
                target: "/workspaces/test-apple-workspace".to_string(),
            }),
            extra_args: vec![],
            entrypoint: None,
            init: false,
            privileged: false,
            cap_add: vec![],
            security_opt: vec![],
        };

        // Clean up any previous test container (using truncated ID)
        let _ = runtime.client.stop(&truncated_id).await;
        let _ = runtime.client.delete(&truncated_id, true).await;

        // Use the public API (same as dev up)
        let id = runtime.create_container(&container_config).await.expect("create_container failed");
        assert_eq!(id, truncated_id, "create_container should return truncated ID");
        runtime.start_container(&id).await.expect("start_container failed");

        // Verify running
        let snapshot = runtime.client.get(&id).await.expect("get failed");
        assert_eq!(snapshot.status, RuntimeStatus::Running, "container should be running");

        // Clean up
        runtime.client.stop(&truncated_id).await.expect("stop failed");
        runtime.client.delete(&truncated_id, true).await.expect("delete failed");
    }
}
