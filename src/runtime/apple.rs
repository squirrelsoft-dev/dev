use std::os::fd::AsRawFd;
use std::path::Path;

use apple_container::models::{
    ContainerConfiguration, Filesystem, ImageDescription, OciDescriptor, ProcessConfiguration,
    PublishPort, Resources, RuntimeStatus, User,
};
use apple_container::AppleContainerClient;

use crate::error::DevError;
use crate::runtime::{
    BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState, ExecResult,
    ImageMetadata,
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
fn to_apple_config(config: &ContainerConfig) -> ContainerConfiguration {
    let mounts: Vec<Filesystem> = config
        .mounts
        .iter()
        .map(|m| Filesystem {
            source: m.source.display().to_string(),
            destination: m.target.clone(),
            read_only: m.readonly,
        })
        .chain(config.workspace_mount.iter().map(|ws| Filesystem {
            source: ws.source.display().to_string(),
            destination: ws.target.clone(),
            read_only: false,
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

    let env: Vec<String> = config
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let init_process = ProcessConfiguration {
        executable: config
            .entrypoint
            .clone()
            .unwrap_or_else(|| "/bin/sleep".to_string()),
        arguments: if config.entrypoint.is_none() {
            vec!["infinity".to_string()]
        } else {
            Vec::new()
        },
        environment: env,
        working_directory: "/".to_string(),
        terminal: false,
        user: User::default(),
    };

    ContainerConfiguration {
        id: config.name.clone(),
        image: ImageDescription {
            descriptor: OciDescriptor::default(),
            reference: config.image.clone(),
            manifest_digest: String::new(),
        },
        mounts,
        published_ports,
        labels: config.labels.clone(),
        init_process,
        resources: Resources::default(),
    }
}

impl ContainerRuntime for AppleRuntime {
    fn pull_image(&self, _image: &str) -> BoxFut<'_, ()> {
        // Apple Containers pulls images automatically during containerCreate.
        Box::pin(async { Ok(()) })
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
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
            let apple_config = to_apple_config(&config);
            let id = apple_config.id.clone();

            let kernel = self.client
                .get_default_kernel()
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to get default kernel: {e}")))?;

            self.client
                .create(&apple_config, &kernel)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to create container: {e}")))?;

            // Bootstrap the container with /dev/null fds since the init process
            // runs detached (sleep infinity or the configured entrypoint).
            let devnull = std::fs::File::open("/dev/null")
                .map_err(|e| DevError::Runtime(format!("Failed to open /dev/null: {e}")))?;
            let fd = devnull.as_raw_fd();

            self.client
                .bootstrap(&id, fd, fd, fd)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to bootstrap container: {e}")))?;

            Ok(id)
        })
    }

    fn start_container(&self, _id: &str) -> BoxFut<'_, ()> {
        // Apple containers start on bootstrap — this is a no-op.
        Box::pin(async { Ok(()) })
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
                user: User { uid, gid: uid },
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
                user: User { uid, gid: uid },
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

    fn list_containers(&self, label_filter: &str) -> BoxFut<'_, Vec<ContainerInfo>> {
        let label_filter = label_filter.to_string();
        Box::pin(async move {
            let snapshots = self
                .client
                .list()
                .await
                .map_err(|e| DevError::Runtime(format!("list failed: {e}")))?;

            // Parse the label filter "key=value" and filter client-side.
            let (filter_key, filter_value) = label_filter
                .split_once('=')
                .unwrap_or((&label_filter, ""));

            let mut result = Vec::new();
            for snap in snapshots {
                let matches = snap
                    .configuration
                    .labels
                    .get(filter_key)
                    .is_some_and(|v| filter_value.is_empty() || v == filter_value);

                if matches {
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

    fn inspect_image_metadata(&self, _image: &str) -> BoxFut<'_, ImageMetadata> {
        // Apple Containers doesn't have a local image store to query.
        Box::pin(async { Ok(ImageMetadata::default()) })
    }
}
