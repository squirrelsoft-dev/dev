use bollard::Docker;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecResults};
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    BuildImageOptions, BuilderVersion, CreateContainerOptions, CreateImageOptions,
    ListContainersOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;

use crate::error::DevError;
use crate::runtime::{
    AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
    ExecResult, ImageMetadata, terminal_size,
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
            return Err(DevError::Runtime(
                "Failed to get terminal attributes".into(),
            ));
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

/// Adapts bollard's `LogOutput` stream into an `AsyncRead` byte stream.
struct LogOutputStream {
    inner: std::pin::Pin<
        Box<
            dyn futures_util::Stream<
                    Item = Result<bollard::container::LogOutput, bollard::errors::Error>,
                > + Send,
        >,
    >,
    buffer: bytes::BytesMut,
}

impl tokio::io::AsyncRead for LogOutputStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.buffer.is_empty() {
            let n = std::cmp::min(buf.remaining(), self.buffer.len());
            buf.put_slice(&self.buffer.split_to(n));
            return std::task::Poll::Ready(Ok(()));
        }

        match self.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(log_output))) => {
                let data = match log_output {
                    bollard::container::LogOutput::StdOut { message } => message,
                    bollard::container::LogOutput::StdErr { message } => message,
                    _ => return self.poll_read(cx, buf),
                };
                let n = std::cmp::min(buf.remaining(), data.len());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.buffer.extend_from_slice(&data[n..]);
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                std::task::Poll::Ready(Err(std::io::Error::other(e)))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Shared bollard-backed runtime used by both Docker and Podman.
pub struct BollardRuntime {
    client: Docker,
}

/// Whether the daemon refused this exec because the image has no such
/// executable, rather than being unable to run one at all.
///
/// The daemon declines to start an exec whose executable is not in the image,
/// so a missing shell arrives as an API error rather than as a non-zero exit
/// status — but only an error the docker *server* answered with can mean that.
/// Every transport failure — a restarted Docker Desktop, a moved socket — is a
/// different variant, and matching those on text alone would read a bare ENOENT
/// (`DevError::Io` is `#[error(transparent)]`, so it stringifies to exactly "No
/// such file or directory") as an image without a shell, announcing readiness
/// for a container nothing can reach.
///
/// A server error is not enough on its own either, because the OCI runtime
/// reports every failure of the same start step through the same variant, and
/// several of them carry ENOENT without the executable being the reason: `error
/// setting cwd to "/srv/app": no such file or directory` for a `workspaceFolder`
/// the image does not have, `open /dev/pts/0: no such file or directory` for a
/// broken terminal. Nothing ran in either case, so the text has to be
/// attributable to the command itself: either the runtime says so outright
/// (`executable file not found in $PATH`), or the not-found is reported inside
/// the `exec: "<argv0>": …` clause that names the executable it tried.
fn reports_missing_command(error: &DevError) -> bool {
    let DevError::Bollard(bollard::errors::Error::DockerResponseServerError { message, .. }) =
        error
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    if message.contains("executable file not found") {
        return true;
    }
    message
        .split_once("exec: ")
        .is_some_and(|(_, named)| named.contains("no such file or directory"))
}

/// How long an interactive exec's status is waited for once its streams close.
///
/// Only spent when the streams outlive the process's own exit record, which is
/// milliseconds in practice — the shell exits as soon as it sees the EOF that
/// ended the session. A budget rather than an unbounded wait so a process that
/// keeps running with no stdin cannot park `dev shell` forever.
const EXEC_STATUS_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// The exit code an `inspect_exec` reply carries, if it carries a final one.
///
/// `None` means "ask again": docker reports a still-running exec with no code,
/// and records the code slightly after the process ends, so both states are
/// answers about an exec that has not finished being reported rather than an
/// exec that succeeded.
fn recorded_exit_code(running: Option<bool>, exit_code: Option<i64>) -> Option<i32> {
    if running == Some(true) {
        return None;
    }
    exit_code.map(|code| code as i32)
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
            from_image: Some(image.to_string()),
            ..Default::default()
        };
        // Pass a default (empty) DockerCredentials instead of None. When None
        // is passed, bollard sends an empty X-Registry-Auth header value which
        // Podman rejects as invalid JSON.
        let mut stream = self.client.create_image(
            Some(opts),
            None,
            Some(bollard::auth::DockerCredentials::default()),
        );
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
        build_args: &HashMap<String, String>,
        no_cache: bool,
        verbose: bool,
    ) -> Result<(), DevError> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
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
            .map_err(|e| {
                DevError::BuildFailed(format!("Failed to add Dockerfile to archive: {e}"))
            })?;

        let encoder = archive
            .into_inner()
            .map_err(|e| DevError::BuildFailed(format!("Failed to finalize archive: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| DevError::BuildFailed(format!("Failed to compress context: {e}")))?;

        // When the Dockerfile uses a BuildKit syntax directive (e.g.
        // `# syntax=docker/dockerfile:1`) or BuildKit features like
        // `RUN --mount=`, request the BuildKit builder. Bollard defaults
        // to BuilderV1 which explicitly forces the legacy builder—even on
        // Docker 23+ where BuildKit is otherwise the default—and the
        // legacy builder cannot handle RUN --mount.
        let needs_buildkit = dockerfile.starts_with("# syntax=") || dockerfile.contains("--mount=");
        let version = if needs_buildkit {
            BuilderVersion::BuilderBuildKit
        } else {
            BuilderVersion::BuilderV1
        };

        let is_buildkit = version == BuilderVersion::BuilderBuildKit;

        let mut opts = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: Some(tag.to_string()),
            nocache: no_cache,
            rm: true,
            buildargs: Some(build_args.clone()),
            version,
            ..Default::default()
        };

        // BuildKit requires a unique gRPC session ID for client-daemon communication.
        if is_buildkit {
            opts.session = Some(format!("dev-{}", std::process::id()));
        }

        // Pass an empty credentials map instead of None. When None is passed,
        // bollard sends an empty X-Registry-Config header value which Podman
        // rejects as invalid JSON.
        let body = bollard::body_full(compressed.into());
        let mut stream = self
            .client
            .build_image(opts, Some(HashMap::new()), Some(body));
        while let Some(result) = stream.next().await {
            let info = match result {
                Ok(info) => info,
                Err(e) => {
                    return Err(DevError::BuildFailed(format!("Docker stream error: {e}")));
                }
            };
            if verbose && let Some(ref stream_text) = info.stream {
                eprint!("{stream_text}");
            }
            // Check for BuildKit trace messages with vertex errors or log output.
            if is_buildkit
                && let Some(bollard::models::BuildInfoAux::BuildKit(ref status)) = info.aux
            {
                for vertex in &status.vertexes {
                    if !vertex.error.is_empty() {
                        return Err(DevError::BuildFailed(vertex.error.clone()));
                    }
                }
                if verbose {
                    for log in &status.logs {
                        let text = String::from_utf8_lossy(&log.msg);
                        eprint!("{text}");
                    }
                }
            }
            if let Some(ref detail) = info.error_detail {
                let msg = detail.message.clone().unwrap_or_default();
                if !msg.is_empty() {
                    return Err(DevError::BuildFailed(msg));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn create_container_impl(
        &self,
        config: &ContainerConfig,
    ) -> Result<String, DevError> {
        let container_config = Self::to_create_body(config);

        let opts = CreateContainerOptions {
            name: Some(config.name.clone()),
            ..Default::default()
        };

        let response = self
            .client
            .create_container(Some(opts), container_config)
            .await?;

        Ok(response.id)
    }

    /// Build the daemon-facing create body from our generic container config.
    fn to_create_body(config: &ContainerConfig) -> ContainerCreateBody {
        let mut binds = Vec::new();
        for m in &config.mounts {
            let ro = if m.readonly { ":ro" } else { "" };
            binds.push(format!("{}:{}{ro}", m.source.display(), m.target));
        }
        for v in &config.volumes {
            let ro = if v.readonly { ":ro" } else { "" };
            binds.push(format!("{}:{}{ro}", v.name, v.target));
        }
        if let Some(ws) = &config.workspace_mount {
            binds.push(format!("{}:{}", ws.source.display(), ws.target));
        }

        let env: Vec<String> = config.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        let exposed_ports: Vec<String> = config
            .ports
            .iter()
            .map(|p| format!("{}/tcp", p.container))
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
            init: if config.init { Some(true) } else { None },
            privileged: if config.privileged { Some(true) } else { None },
            cap_add: if config.cap_add.is_empty() {
                None
            } else {
                Some(config.cap_add.clone())
            },
            security_opt: if config.security_opt.is_empty() {
                None
            } else {
                Some(config.security_opt.clone())
            },
            userns_mode: config.userns_mode.clone(),
            ..Default::default()
        };

        let labels: HashMap<&str, &str> = config
            .labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        ContainerCreateBody {
            image: Some(config.image.clone()),
            labels: Some(
                labels
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            env: Some(env),
            exposed_ports: Some(exposed_ports),
            // The resolved workspaceFolder becomes the container's WorkingDir,
            // which every `docker exec` without an explicit one inherits — so
            // lifecycle hooks and `dev exec` run where the devcontainer spec
            // says they should, matching the Apple runtime.
            working_dir: config.workspace_folder.clone(),
            host_config: Some(host_config),
            entrypoint: config.entrypoint.as_ref().map(|ep| vec![ep.clone()]),
            // Keep the container running with a default command if no entrypoint provided.
            cmd: if config.entrypoint.is_none() {
                Some(vec!["sleep".to_string(), "infinity".to_string()])
            } else {
                None
            },
            ..Default::default()
        }
    }

    async fn start_container_impl(&self, id: &str) -> Result<(), DevError> {
        self.client
            .start_container(id, None::<StartContainerOptions>)
            .await?;
        Ok(())
    }

    async fn stop_container_impl(&self, id: &str) -> Result<(), DevError> {
        self.client
            .stop_container(
                id,
                Some(StopContainerOptions {
                    t: Some(10),
                    ..Default::default()
                }),
            )
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
    ) -> Result<i32, DevError> {
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
            // Set the initial terminal size so TUI apps fill the real terminal.
            if let Some((cols, rows)) = terminal_size() {
                let _ = self
                    .client
                    .resize_exec(
                        &exec.id,
                        ResizeExecOptions {
                            height: rows,
                            width: cols,
                        },
                    )
                    .await;
            }

            // Forward SIGWINCH to the container exec so resizes propagate.
            let resize_client = self.client.clone();
            let resize_exec_id = exec.id.clone();
            let sigwinch_handle = tokio::spawn(async move {
                let mut sig = match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::window_change(),
                ) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                while sig.recv().await.is_some() {
                    if let Some((cols, rows)) = terminal_size() {
                        let _ = resize_client
                            .resize_exec(
                                &resize_exec_id,
                                ResizeExecOptions {
                                    height: rows,
                                    width: cols,
                                },
                            )
                            .await;
                    }
                }
            });

            // Create a self-pipe so we can cancel the blocking stdin reader.
            // Writing to cancel_writer causes the poll() in the reader to
            // wake up and exit cleanly, avoiding a stuck blocking thread that
            // would prevent the tokio runtime from shutting down.
            let (cancel_reader, cancel_writer) =
                os_pipe::pipe().map_err(|e| DevError::Runtime(format!("pipe: {e}")))?;

            // Channel to bridge blocking stdin reads → async container writes.
            let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);

            // Blocking stdin reader using poll() so it can be cancelled.
            let stdin_reader_handle = std::thread::spawn(move || {
                use std::os::fd::AsRawFd;
                let stdin_fd = std::io::stdin().as_raw_fd();
                let cancel_fd = cancel_reader.as_raw_fd();
                let mut buf = [0u8; 1024];
                loop {
                    let mut pfds = [
                        libc::pollfd {
                            fd: stdin_fd,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: cancel_fd,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    let ready = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
                    if ready < 0 {
                        break;
                    }
                    if pfds[1].revents & libc::POLLIN != 0 {
                        break;
                    }
                    if pfds[0].revents & libc::POLLIN != 0 {
                        let n = unsafe {
                            libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                        if n <= 0 {
                            break;
                        }
                        let data = translate_shift_enter(&buf[..n as usize]);
                        if stdin_tx.blocking_send(data).is_err() {
                            break;
                        }
                    }
                }
            });

            // Async task that forwards channel data to the container stdin.
            let mut stdin_writer = input;
            let stdin_handle = tokio::spawn(async move {
                while let Some(data) = stdin_rx.recv().await {
                    if stdin_writer.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });

            // Forward container output to our stdout.
            // With TTY mode, stdout and stderr are multiplexed on the same stream.
            let output_handle = tokio::spawn(async move {
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
            });

            // Poll the exec status so we detect when the shell exits even if
            // the output stream stays open (Docker keeps the bidirectional
            // connection alive while stdin is still attached).
            let monitor_client = self.client.clone();
            let monitor_exec_id = exec.id.clone();
            let monitor_handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if let Ok(info) = monitor_client.inspect_exec(&monitor_exec_id).await
                        && info.running == Some(false)
                    {
                        break;
                    }
                }
            });

            // Wait for any exit signal: output stream closing, exec process
            // exiting, or stdin failing.
            let stdin_abort = stdin_handle.abort_handle();
            let output_abort = output_handle.abort_handle();
            let monitor_abort = monitor_handle.abort_handle();
            let sigwinch_abort = sigwinch_handle.abort_handle();

            tokio::select! {
                _ = output_handle => {}
                _ = monitor_handle => {}
                _ = stdin_handle => {}
            }

            // Signal the blocking stdin reader to exit, then clean up.
            drop(cancel_writer);
            let _ = stdin_reader_handle.join();

            stdin_abort.abort();
            output_abort.abort();
            monitor_abort.abort();
            sigwinch_abort.abort();
        }

        let exit_code = self.recorded_exec_status(&exec.id).await?;

        // _raw_guard is dropped here, restoring the terminal.
        Ok(exit_code)
    }

    /// The status the interactive exec finished with.
    ///
    /// The stream loop can end while the process is still running: a piped or
    /// redirected stdin reaches EOF, which ends the forwarding task and wins
    /// the `select!` above, and only then does dropping the input half give the
    /// container's shell its own EOF. Reading `inspect_exec` once at that
    /// moment answers `running: true` with no code recorded, so the status is
    /// polled until docker has one. `dev shell` turns this into its own exit
    /// status, so neither a transport failure nor a status docker never
    /// recorded may pass for success.
    async fn recorded_exec_status(&self, exec_id: &str) -> Result<i32, DevError> {
        let deadline = std::time::Instant::now() + EXEC_STATUS_BUDGET;
        loop {
            let info = self.client.inspect_exec(exec_id).await?;
            if let Some(code) = recorded_exit_code(info.running, info.exit_code) {
                return Ok(code);
            }
            if std::time::Instant::now() >= deadline {
                return Err(DevError::Runtime(format!(
                    "the interactive session ended but docker reported no exit status for it \
                     within {}s",
                    EXEC_STATUS_BUDGET.as_secs()
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn exec_attached_impl(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> Result<AttachedExec, DevError> {
        let exec = self
            .client
            .create_exec(
                id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    user: user.map(|u| u.to_string()),
                    ..Default::default()
                },
            )
            .await?;

        let start = self.client.start_exec(&exec.id, None).await?;
        match start {
            StartExecResults::Attached { output, input } => Ok(AttachedExec {
                stdin: Box::pin(input),
                stdout: Box::pin(LogOutputStream {
                    inner: Box::pin(output),
                    buffer: bytes::BytesMut::new(),
                }),
            }),
            StartExecResults::Detached => Err(DevError::Runtime(
                "exec session detached unexpectedly".into(),
            )),
        }
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
        label_filters: &[String],
    ) -> Result<Vec<ContainerInfo>, DevError> {
        let filters: HashMap<String, Vec<String>> =
            HashMap::from([("label".to_string(), label_filters.to_vec())]);
        let opts = ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        };

        let containers = self.client.list_containers(Some(opts)).await?;
        let mut result = Vec::new();

        for c in containers {
            let state = match c.state {
                Some(bollard::models::ContainerSummaryStateEnum::RUNNING) => {
                    ContainerState::Running
                }
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

    async fn inspect_image_metadata_impl(&self, image: &str) -> Result<ImageMetadata, DevError> {
        let inspect = match self.client.inspect_image(image).await {
            Ok(v) => v,
            Err(_) => return Ok(ImageMetadata::default()),
        };

        let config = match inspect.config {
            Some(c) => c,
            None => return Ok(ImageMetadata::default()),
        };

        let mut remote_user: Option<String> = None;
        let mut container_user: Option<String> = None;
        let mut metadata_entries: Vec<serde_json::Value> = Vec::new();

        // Parse the devcontainer.metadata label (JSON array or single object).
        if let Some(ref labels) = config.labels
            && let Some(raw) = labels.get("devcontainer.metadata")
        {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(raw) {
                metadata_entries = arr;
            } else if let Ok(obj) = serde_json::from_str::<serde_json::Value>(raw) {
                metadata_entries = vec![obj];
            }

            // Later entries win.
            for entry in &metadata_entries {
                if let Some(u) = entry.get("remoteUser").and_then(|v| v.as_str()) {
                    remote_user = Some(u.to_string());
                }
                if let Some(u) = entry.get("containerUser").and_then(|v| v.as_str()) {
                    container_user = Some(u.to_string());
                }
            }
        }

        // Fall back to the Dockerfile USER instruction for container_user.
        if container_user.is_none()
            && let Some(ref user) = config.user
        {
            let user = user.trim();
            if !user.is_empty() {
                container_user = Some(user.to_string());
            }
        }

        Ok(ImageMetadata {
            remote_user,
            container_user,
            metadata_entries,
            env: Vec::new(),
        })
    }
}

impl ContainerRuntime for BollardRuntime {
    fn runtime_name(&self) -> &'static str {
        "docker"
    }

    fn pull_image(&self, image: &str) -> BoxFut<'_, ()> {
        let image = image.to_string();
        Box::pin(async move { self.pull_image_impl(&image).await })
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        build_args: &HashMap<String, String>,
        no_cache: bool,
        verbose: bool,
    ) -> BoxFut<'_, ()> {
        let dockerfile = dockerfile.to_string();
        let context = context.to_path_buf();
        let tag = tag.to_string();
        let build_args = build_args.clone();
        Box::pin(async move {
            self.build_image_impl(&dockerfile, &context, &tag, &build_args, no_cache, verbose)
                .await
        })
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

    fn exec_reports_missing_command(&self, error: &DevError) -> bool {
        reports_missing_command(error)
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, i32> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move { self.exec_interactive_impl(&id, &cmd, user.as_deref()).await })
    }

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo> {
        let id = id.to_string();
        Box::pin(async move { self.inspect_container_impl(&id).await })
    }

    fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
        let label_filters = label_filters.to_vec();
        Box::pin(async move { self.list_containers_impl(&label_filters).await })
    }

    fn image_exists(&self, image: &str) -> BoxFut<'_, bool> {
        let image = image.to_string();
        Box::pin(async move { Ok(self.client.inspect_image(&image).await.is_ok()) })
    }

    fn inspect_image_metadata(&self, image: &str) -> BoxFut<'_, ImageMetadata> {
        let image = image.to_string();
        Box::pin(async move { self.inspect_image_metadata_impl(&image).await })
    }

    fn exec_attached(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> BoxFut<'_, AttachedExec> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move { self.exec_attached_impl(&id, &cmd, user.as_deref()).await })
    }
}

/// Docker-specific runtime (uses default Docker socket).
pub struct DockerRuntime(pub(crate) BollardRuntime);

impl DockerRuntime {
    pub fn connect() -> Result<Self, DevError> {
        Ok(Self(BollardRuntime::connect_default()?))
    }

    /// Try additional Docker Desktop socket paths (macOS puts the real socket
    /// at ~/.docker/run/docker.sock while /var/run/docker.sock may be a
    /// symlink to a different runtime).
    pub fn connect_fallback() -> Option<Self> {
        if cfg!(target_os = "macos")
            && let Ok(home) = std::env::var("HOME")
        {
            let path = format!("{home}/.docker/run/docker.sock");
            if std::path::Path::new(&path).exists() {
                return BollardRuntime::connect_to_socket(&path).ok().map(Self);
            }
        }
        None
    }

    pub async fn ping(&self) -> Result<(), DevError> {
        self.0.ping().await
    }
}

impl ContainerRuntime for DockerRuntime {
    fn runtime_name(&self) -> &'static str {
        "docker"
    }

    fn pull_image(&self, image: &str) -> BoxFut<'_, ()> {
        self.0.pull_image(image)
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        build_args: &HashMap<String, String>,
        no_cache: bool,
        verbose: bool,
    ) -> BoxFut<'_, ()> {
        self.0
            .build_image(dockerfile, context, tag, build_args, no_cache, verbose)
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

    fn exec_reports_missing_command(&self, error: &DevError) -> bool {
        self.0.exec_reports_missing_command(error)
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, i32> {
        self.0.exec_interactive(id, cmd, user)
    }

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo> {
        self.0.inspect_container(id)
    }

    fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
        self.0.list_containers(label_filters)
    }

    fn image_exists(&self, image: &str) -> BoxFut<'_, bool> {
        self.0.image_exists(image)
    }

    fn inspect_image_metadata(&self, image: &str) -> BoxFut<'_, ImageMetadata> {
        self.0.inspect_image_metadata(image)
    }

    fn exec_attached(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> BoxFut<'_, AttachedExec> {
        self.0.exec_attached(id, cmd, user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::WorkspaceMount;

    fn container_config(workspace_folder: Option<&str>) -> ContainerConfig {
        ContainerConfig {
            image: "ubuntu:24.04".to_string(),
            name: "vsc-test".to_string(),
            labels: HashMap::new(),
            env: HashMap::new(),
            mounts: vec![],
            volumes: vec![],
            ports: vec![],
            workspace_mount: Some(WorkspaceMount {
                source: std::path::PathBuf::from("/host/monorepo"),
                target: "/srv/app".to_string(),
            }),
            workspace_folder: workspace_folder.map(str::to_string),
            extra_args: vec![],
            entrypoint: None,
            init: false,
            privileged: false,
            cap_add: vec![],
            security_opt: vec![],
            userns_mode: None,
        }
    }

    /// The container's `WorkingDir` is the resolved `workspaceFolder`, which
    /// may be a subdirectory of the workspace mount. Every `docker exec` that
    /// does not name its own working directory inherits this, so lifecycle
    /// hooks and `dev exec` run where the devcontainer spec says — the same
    /// place the Apple runtime uses.
    #[test]
    fn create_body_runs_in_the_resolved_workspace_folder() {
        let body = BollardRuntime::to_create_body(&container_config(Some("/srv/app/packages/api")));

        assert_eq!(body.working_dir.as_deref(), Some("/srv/app/packages/api"));
        assert_eq!(
            body.host_config
                .and_then(|h| h.binds)
                .and_then(|b| b.first().cloned()),
            Some("/host/monorepo:/srv/app".to_string()),
            "the source tree is still bound at the workspaceMount target"
        );
    }

    /// Without a resolved folder the field stays unset, so the daemon keeps
    /// using the image's own `WorkingDir`.
    #[test]
    fn create_body_leaves_working_dir_unset_without_a_workspace_folder() {
        let body = BollardRuntime::to_create_body(&container_config(None));
        assert_eq!(body.working_dir, None);
    }

    /// The env map `dev up` assembles — `containerEnv`/`remoteEnv` plus the
    /// `runArgs` environment inputs (env-file/`--env`/`-e`) — must reach the
    /// bollard create request as `KEY=VALUE` strings. This is create-body
    /// coverage, not a live-daemon integration test.
    #[test]
    fn create_body_carries_env_entries_as_key_value_strings() {
        let mut cfg = container_config(None);
        cfg.env.insert("FROM_FILE".to_string(), "true".to_string());
        cfg.env.insert("FROM_FLAG".to_string(), "yes".to_string());
        cfg.env.insert("EMPTY".to_string(), String::new());

        let body = BollardRuntime::to_create_body(&cfg);
        let env = body.env.expect("env should be set on the create body");

        assert!(
            env.iter().any(|e| e == "FROM_FILE=true"),
            "env-file entry must reach the daemon body, got {env:?}"
        );
        assert!(
            env.iter().any(|e| e == "FROM_FLAG=yes"),
            "--env flag entry must reach the daemon body, got {env:?}"
        );
        assert!(
            env.iter().any(|e| e == "EMPTY="),
            "empty-valued env entry must reach the daemon body, got {env:?}"
        );
    }

    /// Runtime-option runArgs are translated into existing `ContainerConfig`
    /// fields and then into the bollard HostConfig. This pins the shared
    /// Docker/Podman create-body conversion seam without claiming daemon-level
    /// execution.
    #[test]
    fn create_body_carries_runtime_option_fields() {
        let mut cfg = container_config(None);
        cfg.cap_add = vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()];
        cfg.security_opt = vec![
            "seccomp=unconfined".to_string(),
            "label=disable".to_string(),
        ];
        cfg.privileged = true;
        cfg.init = true;
        cfg.userns_mode = Some("keep-id".to_string());

        let body = BollardRuntime::to_create_body(&cfg);
        let host = body.host_config.expect("host config should be set");

        assert_eq!(
            host.cap_add,
            Some(vec!["SYS_PTRACE".to_string(), "NET_ADMIN".to_string()])
        );
        assert_eq!(
            host.security_opt,
            Some(vec![
                "seccomp=unconfined".to_string(),
                "label=disable".to_string()
            ])
        );
        assert_eq!(host.privileged, Some(true));
        assert_eq!(host.init, Some(true));
        assert_eq!(host.userns_mode.as_deref(), Some("keep-id"));
    }

    fn server_error(message: &str) -> DevError {
        DevError::Bollard(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: message.to_string(),
        })
    }

    /// Only an error the docker server answered with, and that the server
    /// attributed to the executable, reads as "the image has no such
    /// executable". Everything else is the runtime failing to run anything,
    /// and the readiness gate treats a missing command as tolerable — so
    /// misreading one of those announces readiness for a container nothing can
    /// reach.
    #[test]
    fn only_a_server_side_refusal_reads_as_a_missing_command() {
        assert!(reports_missing_command(&server_error(
            "OCI runtime exec failed: exec: \"sh\": executable file not found in $PATH"
        )));
        // An absolute argv[0] the image does not have is reported as ENOENT
        // instead, but still inside the clause naming the executable.
        assert!(reports_missing_command(&server_error(
            "OCI runtime exec failed: exec failed: unable to start container process: \
             exec: \"/opt/bin/sh\": stat /opt/bin/sh: no such file or directory: unknown"
        )));

        // A bare ENOENT from a restarted daemon or a moved socket carries the
        // same words, and `DevError::Io` is transparent so it stringifies to
        // exactly them — but nothing ran, so it must stay fatal.
        assert!(!reports_missing_command(&DevError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound)
        )));
        assert!(!reports_missing_command(&DevError::Runtime(
            "No such file or directory (os error 2)".to_string()
        )));
        assert!(!reports_missing_command(&server_error(
            "no such file or directory"
        )));
        // The same ENOENT from the start step, about the working directory
        // this runtime now sets from `workspaceFolder` rather than about the
        // command. Nothing ran, so readiness must fail rather than warn.
        assert!(!reports_missing_command(&server_error(
            "OCI runtime exec failed: exec failed: unable to start container process: \
             error setting cwd to \"/srv/app/packages/api\": no such file or directory: unknown"
        )));
        assert!(!reports_missing_command(&server_error(
            "OCI runtime exec failed: exec failed: unable to start container process: \
             open /dev/pts/0: no such file or directory: unknown"
        )));
        assert!(!reports_missing_command(&server_error(
            "container is not running"
        )));
    }

    /// `detect_runtime` only ever hands out `DockerRuntime`, so a classifier
    /// the wrapper forgets to forward is a classifier that never runs: the
    /// trait's default answers `false` and `dev up` fails on a shell-less
    /// image instead of tolerating it. Asked through `dyn ContainerRuntime` so
    /// the wrapper's own table is what answers.
    #[test]
    fn the_docker_wrapper_forwards_the_missing_command_classifier() {
        // A path rather than a live daemon: bollard only checks that the socket
        // is there, and nothing here sends a request over it.
        let socket = tempfile::NamedTempFile::new().expect("stand-in socket");
        let runtime = DockerRuntime(
            BollardRuntime::connect_to_socket(&socket.path().to_string_lossy())
                .expect("building a docker client must not need a daemon"),
        );
        let runtime: &dyn ContainerRuntime = &runtime;

        assert!(runtime.exec_reports_missing_command(&server_error(
            "OCI runtime exec failed: exec: \"sh\": executable file not found in $PATH"
        )));
        assert!(!runtime.exec_reports_missing_command(&server_error("container is not running")));
    }

    /// The interactive session's streams can close before docker has recorded
    /// the process's exit — a redirected stdin reaching EOF ends the session
    /// while the shell is still running — so neither state may be read as a
    /// success `dev shell` would then exit with.
    #[test]
    fn an_exec_status_is_only_final_once_the_process_has_stopped() {
        assert_eq!(recorded_exit_code(Some(false), Some(7)), Some(7));
        assert_eq!(recorded_exit_code(Some(false), Some(0)), Some(0));
        assert_eq!(recorded_exit_code(None, Some(3)), Some(3));

        assert_eq!(
            recorded_exit_code(Some(true), None),
            None,
            "a running exec has not reported a status yet"
        );
        assert_eq!(
            recorded_exit_code(Some(true), Some(0)),
            None,
            "a running exec's zero is a placeholder, not its status"
        );
        assert_eq!(
            recorded_exit_code(Some(false), None),
            None,
            "a stopped exec whose code is not recorded yet is not a success"
        );
    }
}
