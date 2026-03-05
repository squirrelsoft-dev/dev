use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::{BuildImageOptions, CreateImageOptions};
use bollard::Docker;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::DevError;
use crate::runtime::{
    BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState, ExecResult,
};

/// RAII guard that puts the terminal into raw mode and restores it on drop.
struct RawModeGuard {
    original: libc::termios,
    fd: i32,
}

impl RawModeGuard {
    fn enter(_stdin: std::io::Stdin) -> Result<Self, DevError> {
        use std::os::fd::AsRawFd;
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
///
/// Terminals encode Shift+Enter in several ways:
///   - CSI u (kitty/VS Code):  ESC [ 1 3 ; 2 u   (\x1b[13;2u)
///   - xterm modifyOtherKeys:  ESC [ 2 7 ; 2 ; 1 3 ~  (\x1b[27;2;13~)
///
/// Shells inside containers often don't understand these, causing garbled
/// output. We rewrite them to a plain \r which the shell treats as Enter.
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

/// Shared bollard-backed runtime used by both Docker and Podman.
pub struct BollardRuntime {
    client: Docker,
}

impl BollardRuntime {
    /// Connect to a specific socket path.
    pub fn connect_to_socket(socket: &str) -> Result<Self, DevError> {
        let client = Docker::connect_with_socket(socket, 120, bollard::API_DEFAULT_VERSION)
            .map_err(|e| DevError::Runtime(format!("Failed to connect to {socket}: {e}")))?;
        Ok(Self { client })
    }

    /// Connect using the default Docker socket.
    pub fn connect_default() -> Result<Self, DevError> {
        let client = Docker::connect_with_socket_defaults()
            .map_err(|e| DevError::Runtime(format!("Failed to connect to Docker: {e}")))?;
        Ok(Self { client })
    }

    /// Ping the daemon to confirm connectivity.
    pub async fn ping(&self) -> Result<(), DevError> {
        self.client
            .ping()
            .await
            .map_err(|e| DevError::Runtime(format!("Ping failed: {e}")))?;
        Ok(())
    }

    async fn pull_image_impl(&self, image: &str) -> Result<(), DevError> {
        use futures_util::StreamExt;
        let opts = CreateImageOptions {
            from_image: image,
            ..Default::default()
        };
        let mut stream = self.client.create_image(Some(opts), None, None);
        while let Some(result) = stream.next().await {
            result?;
        }
        Ok(())
    }

    async fn build_image_impl(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        no_cache: bool,
        verbose: bool,
    ) -> Result<(), DevError> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use futures_util::StreamExt;

        // Create a tar.gz archive of the build context with the Dockerfile injected.
        let buf = Vec::new();
        let encoder = GzEncoder::new(buf, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        archive
            .append_dir_all(".", context)
            .map_err(|e| DevError::BuildFailed(format!("Failed to archive context: {e}")))?;

        // Inject the Dockerfile content into the archive so the daemon can find it.
        let dockerfile_bytes = dockerfile.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(dockerfile_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "Dockerfile", dockerfile_bytes)
            .map_err(|e| DevError::BuildFailed(format!("Failed to add Dockerfile to archive: {e}")))?;

        let encoder = archive
            .into_inner()
            .map_err(|e| DevError::BuildFailed(format!("Failed to finalize archive: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| DevError::BuildFailed(format!("Failed to compress context: {e}")))?;

        let opts = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: tag.to_string(),
            nocache: no_cache,
            rm: true,
            ..Default::default()
        };

        let mut stream = self.client.build_image(opts, None, Some(compressed.into()));
        while let Some(result) = stream.next().await {
            let info = result.map_err(|e| {
                DevError::BuildFailed(format!("Docker stream error: {e}"))
            })?;
            if verbose {
                if let Some(ref stream_text) = info.stream {
                    eprint!("{stream_text}");
                }
            }
            if let Some(err) = info.error {
                let detail = info.error_detail
                    .and_then(|d| d.message)
                    .unwrap_or_default();
                let msg = if detail.is_empty() {
                    err
                } else {
                    format!("{err}: {detail}")
                };
                return Err(DevError::BuildFailed(msg));
            }
        }
        Ok(())
    }

    async fn create_container_impl(&self, config: &ContainerConfig) -> Result<String, DevError> {
        let mut binds = Vec::new();
        for m in &config.mounts {
            let ro = if m.readonly { ":ro" } else { "" };
            binds.push(format!("{}:{}{ro}", m.source.display(), m.target));
        }
        if let Some(ws) = &config.workspace_mount {
            binds.push(format!("{}:{}", ws.source.display(), ws.target));
        }

        let env: Vec<String> = config
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        let exposed_ports: HashMap<String, HashMap<(), ()>> = config
            .ports
            .iter()
            .map(|p| (format!("{}/tcp", p.container), HashMap::new()))
            .collect();
        let exposed_ports: HashMap<&str, HashMap<(), ()>> = exposed_ports
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

        let port_bindings: HashMap<String, Option<Vec<bollard::models::PortBinding>>> = config
            .ports
            .iter()
            .map(|p| {
                (
                    format!("{}/tcp", p.container),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some(p.host.to_string()),
                    }]),
                )
            })
            .collect();

        let host_config = bollard::models::HostConfig {
            binds: Some(binds),
            port_bindings: Some(port_bindings),
            ..Default::default()
        };

        let labels: HashMap<&str, &str> = config
            .labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let container_config = Config {
            image: Some(config.image.as_str()),
            labels: Some(labels),
            env: Some(env.iter().map(|s| s.as_str()).collect()),
            exposed_ports: Some(exposed_ports),
            host_config: Some(host_config),
            entrypoint: config.entrypoint.as_ref().map(|ep| vec![ep.as_str()]),
            // Keep the container running with a default command if no entrypoint provided.
            cmd: if config.entrypoint.is_none() {
                Some(vec!["sleep", "infinity"])
            } else {
                None
            },
            ..Default::default()
        };

        let opts = CreateContainerOptions {
            name: config.name.as_str(),
            ..Default::default()
        };

        let response = self
            .client
            .create_container(Some(opts), container_config)
            .await?;

        Ok(response.id)
    }

    async fn start_container_impl(&self, id: &str) -> Result<(), DevError> {
        self.client
            .start_container(id, None::<StartContainerOptions<String>>)
            .await?;
        Ok(())
    }

    async fn stop_container_impl(&self, id: &str) -> Result<(), DevError> {
        self.client
            .stop_container(id, Some(StopContainerOptions { t: 10 }))
            .await?;
        Ok(())
    }

    async fn remove_container_impl(&self, id: &str) -> Result<(), DevError> {
        self.client
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }

    async fn exec_impl(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> Result<ExecResult, DevError> {
        use futures_util::StreamExt;

        let exec = self
            .client
            .create_exec(
                id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    user: user.map(|u| u.to_string()),
                    ..Default::default()
                },
            )
            .await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        let start = self.client.start_exec(&exec.id, None).await?;
        if let StartExecResults::Attached { mut output, .. } = start {
            while let Some(msg) = output.next().await {
                let msg = msg?;
                match msg {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.client.inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(-1) as i32;

        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    async fn exec_interactive_impl(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> Result<(), DevError> {
        use futures_util::StreamExt;

        let exec = self
            .client
            .create_exec(
                id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(true),
                    user: user.map(|u| u.to_string()),
                    ..Default::default()
                },
            )
            .await?;

        // Put the local terminal into raw mode so keystrokes (tab, arrows,
        // ctrl-sequences) are forwarded to the container unprocessed.
        let stdin_fd = std::io::stdin();
        let _raw_guard = RawModeGuard::enter(stdin_fd)?;

        let start = self.client.start_exec(&exec.id, None).await?;
        if let StartExecResults::Attached {
            mut output, input, ..
        } = start
        {
            // Spawn a task to forward stdin to the container, translating
            // Shift+Enter escape sequences into a plain carriage return so they
            // don't print garbage in shells that don't understand them.
            let mut stdin_writer = input;
            tokio::spawn(async move {
                let mut stdin = tokio::io::stdin();
                let mut buf = [0u8; 1024];
                loop {
                    let n = match stdin.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let translated = translate_shift_enter(&buf[..n]);
                    if stdin_writer.write_all(&translated).await.is_err() {
                        break;
                    }
                }
            });

            // Forward container output to our stdout.
            // With TTY mode, stdout and stderr are multiplexed on the same stream.
            let mut local_stdout = tokio::io::stdout();
            while let Some(msg) = output.next().await {
                match msg {
                    Ok(bollard::container::LogOutput::StdOut { message }) => {
                        let _ = local_stdout.write_all(&message).await;
                        let _ = local_stdout.flush().await;
                    }
                    Ok(bollard::container::LogOutput::StdErr { message }) => {
                        let _ = local_stdout.write_all(&message).await;
                        let _ = local_stdout.flush().await;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }

        // _raw_guard is dropped here, restoring the terminal.
        Ok(())
    }

    #[allow(dead_code)]
    async fn inspect_container_impl(&self, id: &str) -> Result<ContainerInfo, DevError> {
        let resp = self.client.inspect_container(id, None).await?;

        let state = match resp.state.and_then(|s| s.running) {
            Some(true) => ContainerState::Running,
            _ => ContainerState::Stopped,
        };

        let labels = resp
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default();

        let image = resp
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .unwrap_or_default();

        let name = resp
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();

        Ok(ContainerInfo {
            id: resp.id.unwrap_or_default(),
            name,
            state,
            labels,
            image,
        })
    }

    async fn list_containers_impl(
        &self,
        label_filter: &str,
    ) -> Result<Vec<ContainerInfo>, DevError> {
        let filters: HashMap<&str, Vec<&str>> =
            HashMap::from([("label", vec![label_filter])]);
        let opts = ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        };

        let containers = self.client.list_containers(Some(opts)).await?;
        let mut result = Vec::new();

        for c in containers {
            let state = match c.state.as_deref() {
                Some("running") => ContainerState::Running,
                _ => ContainerState::Stopped,
            };

            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();

            result.push(ContainerInfo {
                id: c.id.unwrap_or_default(),
                name,
                state,
                labels: c.labels.unwrap_or_default(),
                image: c.image.unwrap_or_default(),
            });
        }

        Ok(result)
    }
}

impl ContainerRuntime for BollardRuntime {
    fn pull_image(&self, image: &str) -> BoxFut<'_, ()> {
        let image = image.to_string();
        Box::pin(async move { self.pull_image_impl(&image).await })
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
        Box::pin(async move { self.build_image_impl(&dockerfile, &context, &tag, no_cache, verbose).await })
    }

    fn create_container(&self, config: &ContainerConfig) -> BoxFut<'_, String> {
        let config = config.clone();
        Box::pin(async move { self.create_container_impl(&config).await })
    }

    fn start_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move { self.start_container_impl(&id).await })
    }

    fn stop_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move { self.stop_container_impl(&id).await })
    }

    fn remove_container(&self, id: &str) -> BoxFut<'_, ()> {
        let id = id.to_string();
        Box::pin(async move { self.remove_container_impl(&id).await })
    }

    fn exec(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ExecResult> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move { self.exec_impl(&id, &cmd, user.as_deref()).await })
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ()> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move { self.exec_interactive_impl(&id, &cmd, user.as_deref()).await })
    }

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo> {
        let id = id.to_string();
        Box::pin(async move { self.inspect_container_impl(&id).await })
    }

    fn list_containers(&self, label_filter: &str) -> BoxFut<'_, Vec<ContainerInfo>> {
        let label_filter = label_filter.to_string();
        Box::pin(async move { self.list_containers_impl(&label_filter).await })
    }
}

/// Docker-specific runtime (uses default Docker socket).
pub struct DockerRuntime(pub(crate) BollardRuntime);

impl DockerRuntime {
    pub fn connect() -> Result<Self, DevError> {
        Ok(Self(BollardRuntime::connect_default()?))
    }

    pub async fn ping(&self) -> Result<(), DevError> {
        self.0.ping().await
    }
}

impl ContainerRuntime for DockerRuntime {
    fn pull_image(&self, image: &str) -> BoxFut<'_, ()> {
        self.0.pull_image(image)
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        no_cache: bool,
        verbose: bool,
    ) -> BoxFut<'_, ()> {
        self.0.build_image(dockerfile, context, tag, no_cache, verbose)
    }

    fn create_container(&self, config: &ContainerConfig) -> BoxFut<'_, String> {
        self.0.create_container(config)
    }

    fn start_container(&self, id: &str) -> BoxFut<'_, ()> {
        self.0.start_container(id)
    }

    fn stop_container(&self, id: &str) -> BoxFut<'_, ()> {
        self.0.stop_container(id)
    }

    fn remove_container(&self, id: &str) -> BoxFut<'_, ()> {
        self.0.remove_container(id)
    }

    fn exec(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ExecResult> {
        self.0.exec(id, cmd, user)
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ()> {
        self.0.exec_interactive(id, cmd, user)
    }

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo> {
        self.0.inspect_container(id)
    }

    fn list_containers(&self, label_filter: &str) -> BoxFut<'_, Vec<ContainerInfo>> {
        self.0.list_containers(label_filter)
    }
}
