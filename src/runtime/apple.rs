use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use apple_container::AppleContainerClient;
use apple_container::error::AppleContainerError;
use apple_container::models::{
    ContainerConfiguration, Empty, FSType, Filesystem, ImageDescription, ProcessConfiguration,
    PublishPort, Resources, RuntimeStatus, User, UserId, UserString,
};

use crate::error::DevError;
use crate::runtime::{
    AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
    ExecResult, ImageMetadata, WorkspaceMount, terminal_size,
};

/// Apple Containers runtime using native XPC.
pub struct AppleRuntime {
    client: AppleContainerClient,
    /// Working directory per container id, so repeated execs do not each pay
    /// for a daemon round trip. Populated directly when this process creates
    /// the container, and by one lookup otherwise.
    working_directories: std::sync::Mutex<HashMap<String, String>>,
}

impl AppleRuntime {
    pub fn connect() -> Result<Self, DevError> {
        let client = AppleContainerClient::connect()
            .map_err(|e| DevError::Runtime(format!("Apple Containers: {e}")))?;
        Ok(Self {
            client,
            working_directories: std::sync::Mutex::new(HashMap::new()),
        })
    }

    fn remembered_working_directory(&self, id: &str) -> Option<String> {
        let cache = self
            .working_directories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.get(id).cloned()
    }

    fn remember_working_directory(&self, id: &str, working_directory: &str) {
        let mut cache = self
            .working_directories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.insert(id.to_string(), working_directory.to_string());
    }

    pub async fn ping(&self) -> Result<(), DevError> {
        self.client
            .ping()
            .await
            .map_err(|e| DevError::Runtime(format!("Apple Containers ping failed: {e}")))?;
        Ok(())
    }

    /// Run a command in the container, capturing its output and exit code.
    async fn exec_impl(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> Result<ExecResult, DevError> {
        let working_directory = self.container_working_directory(id).await;
        let proc_config = exec_process_config(cmd, user, false, &working_directory);
        run_captured_process(&self.client, id, &proc_config).await
    }

    /// The working directory the container's own processes use.
    ///
    /// Resolved at most once per container: `create_container` records what it
    /// configured, and any other invocation (`dev exec`, `dev shell`) reads it
    /// back from the daemon once. A lookup failure is reported and degrades to
    /// `/` for that command only — it is never cached, so a transient daemon
    /// error cannot pin the rest of the invocation to the wrong directory.
    async fn container_working_directory(&self, id: &str) -> String {
        if let Some(known) = self.remembered_working_directory(id) {
            return known;
        }

        match self.client.get(id).await {
            Ok(snapshot) => {
                let working_directory = absolute_or_root([Some(
                    snapshot
                        .configuration
                        .init_process
                        .working_directory
                        .as_str(),
                )]);
                self.remember_working_directory(id, &working_directory);
                working_directory
            }
            Err(e) => {
                eprintln!(
                    "Warning: could not read the working directory of container '{id}' \
                     ({e}); running this command from /"
                );
                "/".to_string()
            }
        }
    }

    /// Run a command against the caller's terminal and wait for it to exit.
    async fn exec_interactive_impl(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> Result<i32, DevError> {
        // Resolved before raw mode: this can warn, and a raw terminal needs
        // CRLF to keep such output from staircasing.
        let working_directory = self.container_working_directory(id).await;
        let proc_config = exec_process_config(cmd, user, true, &working_directory);

        let _raw_guard = RawModeGuard::enter()?;
        let process_id = next_process_id("exec-interactive");

        // Hand the real terminal descriptors to the daemon; it drives the pty.
        self.client
            .create_process(
                id,
                &process_id,
                &proc_config,
                std::io::stdin().as_raw_fd(),
                std::io::stdout().as_raw_fd(),
                std::io::stderr().as_raw_fd(),
            )
            .await
            .map_err(|e| DevError::Runtime(format!("exec_interactive failed: {e}")))?;

        // Registered before the start for the same reason a captured exec does
        // it: see `start_with_registered_exit_wait`.
        let exit_wait = self.client.wait_process(id, &process_id);
        tokio::pin!(exit_wait);
        if let Err(e) = start_with_registered_exit_wait(
            "exec_interactive",
            &mut exit_wait,
            self.client.start_process(id, &process_id),
        )
        .await
        {
            let _ = self
                .client
                .kill_process(id, &process_id, libc::SIGKILL)
                .await;
            return Err(e);
        }

        self.resize_to_terminal(id, &process_id).await;
        let exit_code = self
            .attend_interactive_process(id, &process_id, exit_wait)
            .await?;

        // _raw_guard is dropped here, restoring the terminal.
        Ok(exit_code)
    }

    /// Forward window-size changes and signals until the process exits.
    ///
    /// The daemon copies terminal bytes itself, so the only host-side work left
    /// is keeping the guest pty's window size in sync and relaying signals
    /// aimed at this CLI (the raw-mode terminal delivers Ctrl-C to the guest as
    /// a byte, not as a host SIGINT).
    async fn attend_interactive_process<W>(
        &self,
        id: &str,
        process_id: &str,
        mut wait: std::pin::Pin<&mut W>,
    ) -> Result<i32, DevError>
    where
        W: std::future::Future<Output = Result<i32, AppleContainerError>>,
    {
        use tokio::signal::unix::{SignalKind, signal};

        let watch = |kind: SignalKind, name: &str| {
            signal(kind).map_err(|e| DevError::Runtime(format!("watch {name}: {e}")))
        };
        let mut window_change = watch(SignalKind::window_change(), "SIGWINCH")?;
        let mut interrupt = watch(SignalKind::interrupt(), "SIGINT")?;
        let mut terminate = watch(SignalKind::terminate(), "SIGTERM")?;

        loop {
            tokio::select! {
                exited = &mut wait => {
                    return exited.map_err(|e| {
                        DevError::Runtime(format!("exec_interactive wait failed: {e}"))
                    });
                }
                _ = window_change.recv() => self.resize_to_terminal(id, process_id).await,
                _ = interrupt.recv() => self.forward_signal(id, process_id, libc::SIGINT).await,
                _ = terminate.recv() => self.forward_signal(id, process_id, libc::SIGTERM).await,
            }
        }
    }

    /// Best-effort sync of the guest pty's window size with the host terminal.
    async fn resize_to_terminal(&self, id: &str, process_id: &str) {
        let Some((columns, rows)) = terminal_size() else {
            return;
        };
        if let Err(e) = self
            .client
            .resize_process(id, process_id, columns, rows)
            .await
        {
            // Written with CRLF: the terminal is in raw mode here.
            eprint!("\r\nWarning: could not resize container terminal: {e}\r\n");
        }
    }

    /// Best-effort relay of a host signal to the process inside the container.
    async fn forward_signal(&self, id: &str, process_id: &str, signal: i32) {
        if let Err(e) = self.client.kill_process(id, process_id, signal).await {
            eprint!("\r\nWarning: could not deliver signal {signal} to container: {e}\r\n");
        }
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct CachedImageConfig {
    env: Vec<String>,
    user: Option<String>,
    working_dir: Option<String>,
}

/// Read cached OCI image config for a reference, if present.
fn read_cached_config(reference: &str) -> Option<CachedImageConfig> {
    let path = oci_config_cache_dir()
        .join(cache_key(reference))
        .with_extension("json");
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
    std::fs::write(&path, &data).map_err(|e| DevError::Runtime(format!("write cache: {e}")))?;
    Ok(())
}

/// Platform resolver that chooses the first linux/arm64 variant from an image index.
fn linux_arm64_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|entry| {
            entry
                .platform
                .as_ref()
                .is_some_and(|platform| platform.os == "linux" && platform.architecture == "arm64")
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
    let oci_ref: oci_client::Reference =
        reference.parse().map_err(|e: oci_client::ParseError| {
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
        .pull_blob(&oci_ref, manifest.config.digest.as_str(), &mut config_data)
        .await
        .map_err(|e| DevError::Runtime(format!("pull config blob for {reference}: {e}")))?;

    let config_json: serde_json::Value = serde_json::from_slice(&config_data)
        .map_err(|e| DevError::Runtime(format!("parse image config JSON: {e}")))?;

    let cached = CachedImageConfig::from_oci_config(&config_json);
    if let Err(e) = write_cached_config(reference, &cached) {
        eprintln!("Warning: failed to cache OCI config: {e}");
    }
    Ok(cached)
}

impl CachedImageConfig {
    /// Read the runtime fields out of an OCI image config document.
    fn from_oci_config(config_json: &serde_json::Value) -> Self {
        let section = config_json.get("config");
        let string_field = |name: &str| {
            section
                .and_then(|c| c.get(name))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        Self {
            env: section
                .and_then(|c| c.get("Env"))
                .and_then(|e| e.as_array())
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            user: string_field("User"),
            working_dir: string_field("WorkingDir"),
        }
    }
}

/// Read an image's config from the daemon's own content store.
///
/// An image `dev` just built has no registry to query, and for a pulled image
/// the local blob is the exact config the container will run with, so this is
/// both the only source in one case and the more accurate one in the other.
fn read_local_image_config(image: &ImageDescription) -> Option<CachedImageConfig> {
    // Apple Containers runs Linux guests on Apple silicon only, matching the
    // platform every other request in this runtime asks for.
    let config_json =
        apple_container::content::read_image_config(&image.descriptor.digest, "linux", "arm64")?;
    Some(CachedImageConfig::from_oci_config(&config_json))
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

/// Serial number making every exec process identifier unique within this process.
static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Build a process identifier that is unique per exec call.
///
/// The daemon keys processes by this identifier within a container, so two
/// execs sharing one identifier collide. `dev up` runs lifecycle hooks
/// concurrently, so a host-PID-only identifier is not enough.
fn next_process_id(prefix: &str) -> String {
    let seq = EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{seq}", std::process::id())
}

/// Map a devcontainer user spec onto Apple's process user model.
///
/// `remoteUser` is normally a name (`vscode`), which only `User::Raw` can
/// carry — the daemon resolves it against the image's `/etc/passwd`. Numeric
/// `uid` and `uid:gid` specs map onto `User::Id`.
fn to_apple_user(user: Option<&str>) -> User {
    let Some(spec) = user.map(str::trim).filter(|u| !u.is_empty()) else {
        return User::Id {
            id: UserId { uid: 0, gid: 0 },
        };
    };
    match parse_numeric_user(spec) {
        Some(id) => User::Id { id },
        None => User::Raw {
            raw: UserString {
                user_string: spec.to_string(),
            },
        },
    }
}

/// Parse `uid` or `uid:gid`, returning None for anything non-numeric.
fn parse_numeric_user(spec: &str) -> Option<UserId> {
    match spec.split_once(':') {
        Some((uid, gid)) => Some(UserId {
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
        }),
        None => {
            let uid = spec.parse().ok()?;
            Some(UserId { uid, gid: uid })
        }
    }
}

/// Build the process configuration for an exec'd command.
fn exec_process_config(
    cmd: &[String],
    user: Option<&str>,
    terminal: bool,
    working_directory: &str,
) -> ProcessConfiguration {
    ProcessConfiguration {
        executable: cmd.first().cloned().unwrap_or_default(),
        arguments: cmd.get(1..).map(<[String]>::to_vec).unwrap_or_default(),
        environment: Vec::new(),
        working_directory: absolute_or_root([Some(working_directory)]),
        terminal,
        user: to_apple_user(user),
        supplemental_groups: vec![],
        rlimits: vec![],
    }
}

/// First absolute candidate, or `/` when none is usable.
///
/// The daemon has no "unset" working directory: it chdirs into whatever string
/// it is given, so a relative or empty value would fail the process outright.
fn absolute_or_root<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> String {
    candidates
        .into_iter()
        .flatten()
        .find(|dir| dir.starts_with('/'))
        .unwrap_or("/")
        .to_string()
}

/// The directory a container's processes run in.
///
/// Docker and Podman leave the exec working directory unset so it inherits the
/// container's, which comes from the image's `WorkingDir`. Apple's daemon has
/// no such inheritance, so the directory is resolved once here and applied to
/// both the init process and every exec.
///
/// The resolved `workspaceFolder` comes first: that is where the devcontainer
/// spec runs lifecycle commands, and it may be a subdirectory of the workspace
/// mount (one project of a monorepo). The mount destination is the fallback for
/// a container without a resolved folder, and the image's `WorkingDir` for one
/// without a workspace at all.
fn container_working_directory(
    workspace_folder: Option<&str>,
    workspace_mount: Option<&WorkspaceMount>,
    image_working_dir: Option<&str>,
) -> String {
    absolute_or_root([
        workspace_folder,
        workspace_mount.map(|ws| ws.target.as_str()),
        image_working_dir,
    ])
}

/// A boxed daemon future.
type DaemonFut<'a, T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<T, AppleContainerError>> + Send + 'a>,
>;

/// The daemon calls a captured exec drives, behind a seam.
///
/// `create_process` only registers a process; only `start_process` runs it, and
/// only `wait_process` reports its real status. Issue #4 was that sequence
/// collapsing to a bare create with a hardcoded exit code, which no test can
/// observe through a concrete XPC client — so the sequence is expressed against
/// this trait and a stand-in stands in for the daemon in tests.
trait ExecDaemon: Send + Sync {
    fn create_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
        config: &'a ProcessConfiguration,
        stdin: std::os::fd::RawFd,
        stdout: std::os::fd::RawFd,
        stderr: std::os::fd::RawFd,
    ) -> DaemonFut<'a, ()>;

    fn start_process<'a>(&'a self, container_id: &'a str, process_id: &'a str)
    -> DaemonFut<'a, ()>;

    fn wait_process<'a>(&'a self, container_id: &'a str, process_id: &'a str)
    -> DaemonFut<'a, i32>;

    fn kill_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
        signal: i32,
    ) -> DaemonFut<'a, ()>;
}

impl ExecDaemon for AppleContainerClient {
    fn create_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
        config: &'a ProcessConfiguration,
        stdin: std::os::fd::RawFd,
        stdout: std::os::fd::RawFd,
        stderr: std::os::fd::RawFd,
    ) -> DaemonFut<'a, ()> {
        Box::pin(AppleContainerClient::create_process(
            self,
            container_id,
            process_id,
            config,
            stdin,
            stdout,
            stderr,
        ))
    }

    fn start_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
    ) -> DaemonFut<'a, ()> {
        Box::pin(AppleContainerClient::start_process(
            self,
            container_id,
            process_id,
        ))
    }

    fn wait_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
    ) -> DaemonFut<'a, i32> {
        Box::pin(AppleContainerClient::wait_process(
            self,
            container_id,
            process_id,
        ))
    }

    fn kill_process<'a>(
        &'a self,
        container_id: &'a str,
        process_id: &'a str,
        signal: i32,
    ) -> DaemonFut<'a, ()> {
        Box::pin(AppleContainerClient::kill_process(
            self,
            container_id,
            process_id,
            signal,
        ))
    }
}

/// Put a process's exit wait on the wire before its start request.
///
/// `create_process` only registers a process; only `start_process` runs it.
/// The daemon, though, never answers a `containerWait` that reaches it while
/// it is recording that process's exit, so a wait sent after the start is lost
/// whenever the command finishes first — a short command like `echo` usually
/// does, which is the `dev exec` and lifecycle-hook hang in issue #4. A wait
/// registered while the process still cannot exit is always answered, so this
/// polls the wait (which sends its request) before sending the start.
async fn start_with_registered_exit_wait<W>(
    what: &str,
    exit_wait: &mut std::pin::Pin<&mut W>,
    start: impl std::future::Future<Output = Result<(), AppleContainerError>>,
) -> Result<(), DevError>
where
    W: std::future::Future<Output = Result<i32, AppleContainerError>>,
{
    tokio::select! {
        biased;
        // Only a daemon-side failure can answer this early: a process that has
        // not been started has no exit to report. Taking the answer here also
        // keeps the caller from polling a future that has already completed.
        answered = exit_wait.as_mut() => Err(match answered {
            Err(e) => DevError::Runtime(format!("{what} wait failed: {e}")),
            Ok(code) => DevError::Runtime(format!(
                "{what} wait reported exit {code} for a process that was never started"
            )),
        }),
        // The wait's request is on its way; start the process behind it.
        _ = tokio::task::yield_now() => start
            .await
            .map_err(|e| DevError::Runtime(format!("{what} start failed: {e}"))),
    }
}

/// Create, start, and wait for one process, capturing its output.
async fn run_captured_process(
    daemon: &dyn ExecDaemon,
    id: &str,
    proc_config: &ProcessConfiguration,
) -> Result<ExecResult, DevError> {
    let (stdin_read, stdin_write) = exec_pipe()?;
    let (stdout_read, stdout_write) = exec_pipe()?;
    let (stderr_read, stderr_write) = exec_pipe()?;

    let process_id = next_process_id("exec");

    daemon
        .create_process(
            id,
            &process_id,
            proc_config,
            stdin_read.as_raw_fd(),
            stdout_write.as_raw_fd(),
            stderr_write.as_raw_fd(),
        )
        .await
        .map_err(|e| DevError::Runtime(format!("exec failed: {e}")))?;

    // The daemon holds its own copies of all three descriptors now. Closing
    // ours gives the process EOF on stdin — nothing writes to it, so a
    // command that reads stdin would otherwise block forever — and lets the
    // readers below see EOF once the process exits.
    drop(stdin_write);
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);

    // The readers drain on the blocking pool, so a process that fills a pipe
    // buffer can still reach its exit.
    let stdout_task = read_pipe(stdout_read);
    let stderr_task = read_pipe(stderr_read);

    let exit_wait = daemon.wait_process(id, &process_id);
    tokio::pin!(exit_wait);
    let start = daemon.start_process(id, &process_id);
    if let Err(e) = start_with_registered_exit_wait("exec", &mut exit_wait, start).await {
        // The wait is already registered with the daemon and the readers only
        // finish at EOF. Tear the process down so both are released, then
        // report the failure instead of blocking on them.
        let _ = daemon.kill_process(id, &process_id, libc::SIGKILL).await;
        return Err(e);
    }

    let exit_code = match exit_wait.await {
        Ok(code) => code,
        Err(e) => {
            // Waiting failed while the process may still be running, and the
            // readers only finish at EOF. Stop the process so they do, and
            // report the wait failure instead of blocking on it.
            let _ = daemon.kill_process(id, &process_id, libc::SIGKILL).await;
            return Err(DevError::Runtime(format!("exec wait failed: {e}")));
        }
    };

    let (stdout_buf, stderr_buf) = tokio::join!(stdout_task, stderr_task);

    Ok(ExecResult {
        exit_code,
        stdout: finish_read(stdout_buf, "stdout")?,
        stderr: finish_read(stderr_buf, "stderr")?,
    })
}

/// Create a pipe, mapping the OS error into a runtime error.
fn exec_pipe() -> Result<(os_pipe::PipeReader, os_pipe::PipeWriter), DevError> {
    os_pipe::pipe().map_err(|e| DevError::Runtime(format!("pipe: {e}")))
}

/// Drain a pipe to a string on the blocking pool.
fn read_pipe(mut reader: os_pipe::PipeReader) -> tokio::task::JoinHandle<std::io::Result<String>> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = String::new();
        reader.read_to_string(&mut buf).map(|_| buf)
    })
}

/// Unwrap the two error layers a [`read_pipe`] task can fail with.
fn finish_read(
    joined: Result<std::io::Result<String>, tokio::task::JoinError>,
    stream: &str,
) -> Result<String, DevError> {
    joined
        .map_err(|e| DevError::Runtime(format!("{stream} reader join: {e}")))?
        .map_err(|e| DevError::Runtime(format!("read {stream}: {e}")))
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

    merged
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
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
    image_config: &CachedImageConfig,
) -> ContainerConfiguration {
    let mounts: Vec<Filesystem> = config
        .mounts
        .iter()
        .map(|m| Filesystem {
            fs_type: FSType::Virtiofs(Empty {}),
            source: m.source.display().to_string(),
            destination: m.target.clone(),
            options: if m.readonly {
                vec!["ro".to_string()]
            } else {
                vec![]
            },
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

    let init_env = merge_env(&image_config.env, &config.env);

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
        working_directory: container_working_directory(
            config.workspace_folder.as_deref(),
            config.workspace_mount.as_ref(),
            image_config.working_dir.as_deref(),
        ),
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

/// Whether a container's labels satisfy every `key=value` filter.
///
/// AND semantics, with an empty value meaning "any value for this key".
fn labels_match(labels: &HashMap<String, String>, filters: &[(&str, &str)]) -> bool {
    filters.iter().all(|(key, value)| {
        labels
            .get(*key)
            .is_some_and(|v| value.is_empty() || v == value)
    })
}

/// Fail a filtered query the daemon answered with an unreadable record for the
/// very container being asked about, and nothing else.
///
/// Skipping undecodable entries keeps unrelated containers discoverable, but
/// answering "nothing matched" for the caller's *own* container reads as "no
/// such container": `dev up` then creates a duplicate that the daemon rejects
/// with a confusing "already exists" instead of the decode failure that
/// explains it.
///
/// Two cases keep the tolerant behaviour. An unfiltered query asks about every
/// container rather than a specific one. And a query that *did* find a readable
/// container for these labels has what the caller came for — failing it would
/// cost `dev status`, `dev exec`, `dev shell` and `dev down` a container they
/// can reach, leaving the unreadable one impossible to clean up.
fn report_undecodable_match(
    undecodable: &[apple_container::models::UndecodableSnapshot],
    filters: &[(&str, &str)],
    readable_matches: usize,
) -> Result<(), DevError> {
    if filters.is_empty() || readable_matches > 0 {
        return Ok(());
    }
    match undecodable
        .iter()
        .find(|entry| labels_match(&entry.labels, filters))
    {
        Some(entry) => Err(DevError::Runtime(format!(
            "container '{}' belongs to this workspace but the daemon returned a record this \
             version cannot read ({}). Remove it with `container delete {}` and run `dev up` \
             again.",
            entry.id, entry.error, entry.id
        ))),
        None => Ok(()),
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
                None => apple_container::build::pull_image(&config.image, &platform_json)
                    .await
                    .map_err(|e| DevError::Runtime(format!("pull image: {e}")))?,
            };

            apple_container::build::unpack_image(&image_desc_bytes, &platform_json)
                .await
                .map_err(|e| DevError::Runtime(format!("unpack image: {e}")))?;
            let image: ImageDescription = serde_json::from_slice(&image_desc_bytes)
                .map_err(|e| DevError::Runtime(format!("parse image descriptor: {e}")))?;

            // Fetch the OCI image config (cached or from registry) so the init
            // process inherits the image's expected environment and working
            // directory.
            let image_config = if let Some(c) = read_cached_config(&config.image) {
                c
            } else if let Some(c) = read_local_image_config(&image) {
                // The image is already unpacked here, so its config is on disk.
                // An image `dev` built has no registry to fall back to.
                c
            } else if config.image.starts_with("localhost/") {
                // Local images have no registry to query; daemon handles env vars.
                CachedImageConfig::default()
            } else {
                match fetch_and_cache_oci_config(&config.image).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "Warning: could not fetch image config for {}: {e}",
                            config.image
                        );
                        CachedImageConfig::default()
                    }
                }
            };

            let apple_config = to_apple_config(&config, image, &image_config);
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

            let kernel = self
                .client
                .get_default_kernel()
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to get default kernel: {e}")))?;

            self.client
                .create(&apple_config, &kernel)
                .await
                .map_err(|e| DevError::Runtime(format!("Failed to create container: {e}")))?;

            self.remember_working_directory(&id, &apple_config.init_process.working_directory);

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
        Box::pin(async move { self.exec_impl(&id, &cmd, user.as_deref()).await })
    }

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, i32> {
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        Box::pin(async move { self.exec_interactive_impl(&id, &cmd, user.as_deref()).await })
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
            let listing = self
                .client
                .list_all()
                .await
                .map_err(|e| DevError::Runtime(format!("list failed: {e}")))?;

            // Parse each "key=value" filter and require all to match (AND semantics).
            let parsed_filters: Vec<(&str, &str)> = label_filters
                .iter()
                .map(|f| f.split_once('=').unwrap_or((f.as_str(), "")))
                .collect();

            let mut result = Vec::new();
            for snap in listing.snapshots {
                if labels_match(&snap.configuration.labels, &parsed_filters) {
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

            report_undecodable_match(&listing.undecodable, &parsed_filters, result.len())?;

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
    use apple_container::models::{
        DnsInfo, NetworkInfo, NetworkOptions, Platform, UndecodableSnapshot,
    };
    use std::collections::HashMap;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    // ---- issue #4 regression coverage: the exec create → start → wait
    // sequence, driven against a stand-in daemon so it runs in CI ----

    /// The descriptors the daemon keeps once `create_process` has handed them
    /// over. XPC gives the real daemon its own copies, so the fake dups them
    /// and the caller's closes are observable exactly as they are in
    /// production.
    struct HeldStdio {
        stdin: OwnedFd,
        stdout: Option<OwnedFd>,
        stderr: Option<OwnedFd>,
    }

    /// Stand-in for the Apple Containers daemon's process API.
    ///
    /// It reproduces the daemon behaviour behind issue #4's hang: a wait that
    /// arrives after the process has exited is silently dropped and never
    /// answered.
    #[derive(Default)]
    struct FakeExecDaemon {
        calls: Mutex<Vec<&'static str>>,
        held: Mutex<Option<HeldStdio>>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: i32,
        start_fails: bool,
        wait_fails: bool,
        stdin_reached_eof: AtomicBool,
        /// Set once `start_process` has run the process to completion.
        exited: AtomicBool,
    }

    fn dup_fd(fd: RawFd) -> OwnedFd {
        let duplicated = unsafe { libc::dup(fd) };
        assert!(duplicated >= 0, "the fake daemon must be able to dup {fd}");
        unsafe { OwnedFd::from_raw_fd(duplicated) }
    }

    fn write_fd(fd: &OwnedFd, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let written = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
        assert_eq!(written, data.len() as isize, "fake daemon stdio write");
    }

    /// Whether every write end of a pipe has been closed.
    ///
    /// Nothing ever writes to an exec's stdin, so a process that does not see
    /// EOF here is a process that blocks forever.
    fn pipe_is_at_eof(fd: RawFd) -> bool {
        let mut watch = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut watch, 1, 2_000) } <= 0 {
            return false;
        }
        let mut byte = 0u8;
        unsafe { libc::read(fd, std::ptr::addr_of_mut!(byte).cast(), 1) == 0 }
    }

    impl FakeExecDaemon {
        fn producing(stdout: &str, stderr: &str, exit_code: i32) -> Self {
            Self {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                exit_code,
                ..Self::default()
            }
        }

        fn failing_start() -> Self {
            Self {
                start_fails: true,
                ..Self::default()
            }
        }

        fn failing_wait() -> Self {
            Self {
                wait_fails: true,
                ..Self::default()
            }
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        /// Close the daemon's copies of the output pipes, as a process exiting
        /// (or being killed) does.
        fn close_output(&self) {
            if let Some(held) = self.held.lock().unwrap().as_mut() {
                held.stdout.take();
                held.stderr.take();
            }
        }
    }

    impl ExecDaemon for FakeExecDaemon {
        fn create_process<'a>(
            &'a self,
            _container_id: &'a str,
            _process_id: &'a str,
            _config: &'a ProcessConfiguration,
            stdin: RawFd,
            stdout: RawFd,
            stderr: RawFd,
        ) -> DaemonFut<'a, ()> {
            self.record("create");
            *self.held.lock().unwrap() = Some(HeldStdio {
                stdin: dup_fd(stdin),
                stdout: Some(dup_fd(stdout)),
                stderr: Some(dup_fd(stderr)),
            });
            Box::pin(async { Ok(()) })
        }

        /// Each request reaches the daemon when its future is polled, not when
        /// it is built, so the recorded call order is the order the daemon
        /// sees — which is the whole point of registering the wait first.
        fn start_process<'a>(
            &'a self,
            _container_id: &'a str,
            _process_id: &'a str,
        ) -> DaemonFut<'a, ()> {
            Box::pin(async move {
                self.record("start");
                if self.start_fails {
                    return Err(AppleContainerError::XpcError("start refused".to_string()));
                }
                {
                    let held = self.held.lock().unwrap();
                    let held = held.as_ref().expect("start without create");
                    write_fd(held.stdout.as_ref().expect("stdout"), &self.stdout);
                    write_fd(held.stderr.as_ref().expect("stderr"), &self.stderr);
                }
                // The command runs to completion and exits, closing its pipes.
                self.exited.store(true, Ordering::SeqCst);
                self.close_output();
                Ok(())
            })
        }

        fn wait_process<'a>(
            &'a self,
            _container_id: &'a str,
            _process_id: &'a str,
        ) -> DaemonFut<'a, i32> {
            Box::pin(async move {
                self.record("wait");
                let at_eof = {
                    let held = self.held.lock().unwrap();
                    let held = held.as_ref().expect("wait without create");
                    pipe_is_at_eof(held.stdin.as_raw_fd())
                };
                self.stdin_reached_eof.store(at_eof, Ordering::Relaxed);
                if self.wait_fails {
                    return Err(AppleContainerError::XpcError("wait refused".to_string()));
                }
                if self.exited.load(Ordering::SeqCst) {
                    // The exit is already recorded, so this is a wait the
                    // daemon drops: it is never answered.
                    std::future::pending::<()>().await;
                }
                let exit_code = self.exit_code;
                // Answered once the process the caller starts behind it exits.
                std::future::poll_fn(move |_| {
                    if self.exited.load(Ordering::SeqCst) {
                        std::task::Poll::Ready(Ok(exit_code))
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await
            })
        }

        fn kill_process<'a>(
            &'a self,
            _container_id: &'a str,
            _process_id: &'a str,
            _signal: i32,
        ) -> DaemonFut<'a, ()> {
            self.record("kill");
            // A killed process closes its pipes, which is what lets the
            // caller's readers finish instead of blocking at EOF.
            self.close_output();
            Box::pin(async { Ok(()) })
        }
    }

    fn exec_config() -> ProcessConfiguration {
        exec_process_config(
            &["echo".to_string(), "hello".to_string()],
            None,
            false,
            "/tmp",
        )
    }

    /// Run one captured exec, turning a hang into a failure.
    ///
    /// The stand-in daemon drops a late wait exactly as the real one does, so
    /// a regression that goes back to waiting after the start never finishes.
    /// The bound is generous: it only has to be shorter than the harness's
    /// patience, never shorter than a slow machine's exec.
    async fn bounded_exec(daemon: &FakeExecDaemon) -> Result<ExecResult, DevError> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_captured_process(daemon, "cid", &exec_config()),
        )
        .await
        .expect("exec must report the process's exit instead of hanging")
    }

    /// The reported break: `create_process` registers a process but never runs
    /// it, so nothing writes to the output pipes and `dev exec` hangs. A
    /// create-only implementation fails here on the missing `start`.
    #[tokio::test]
    async fn exec_creates_then_starts_then_waits_for_the_process() {
        let daemon = FakeExecDaemon::producing("hello\n", "", 0);
        let result = bounded_exec(&daemon)
            .await
            .expect("a cooperating daemon must produce a result");

        assert_eq!(
            daemon.calls(),
            vec!["create", "wait", "start"],
            "a created process must also be started, with its exit wait already registered"
        );
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
    }

    /// The daemon never answers a `containerWait` that arrives after it has
    /// recorded the process's exit, and a short command exits before a wait
    /// sent after the start can land — that is the `dev up` lifecycle-hook and
    /// `dev exec` hang in issue #4. Registering the wait first is what makes a
    /// command that exits immediately still report its exit.
    #[tokio::test]
    async fn exec_registers_its_exit_wait_before_the_process_can_exit() {
        let daemon = FakeExecDaemon::producing("hello\n", "", 0);
        let result = bounded_exec(&daemon).await.expect("exec");

        assert_eq!(
            daemon.calls().iter().position(|c| *c == "wait"),
            Some(1),
            "the exit wait must be sent between the create and the start, got: {:?}",
            daemon.calls()
        );
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hello\n");
    }

    /// The exit code was hardcoded to 0, so every failing lifecycle hook and
    /// every `dev exec` failure looked like success.
    #[tokio::test]
    async fn exec_reports_the_processes_real_exit_code_and_stderr() {
        let daemon = FakeExecDaemon::producing("", "oops\n", 7);
        let result = bounded_exec(&daemon)
            .await
            .expect("a cooperating daemon must produce a result");

        assert_eq!(result.exit_code, 7, "the real exit code must survive");
        assert_eq!(result.stderr, "oops\n");
        assert!(result.stdout.is_empty());
    }

    /// Nothing writes to an exec's stdin, so both of the caller's ends must be
    /// closed before the process runs — otherwise a command that reads stdin
    /// (`cat`, an `apt-get` prompt) blocks until the container dies.
    #[tokio::test]
    async fn exec_closes_stdin_so_the_process_sees_eof() {
        let daemon = FakeExecDaemon::producing("", "", 0);
        bounded_exec(&daemon).await.expect("exec");

        assert!(
            daemon.stdin_reached_eof.load(Ordering::Relaxed),
            "the process must see EOF on stdin while it is still running"
        );
    }

    /// A failed wait leaves a process running whose pipes the output readers
    /// are still blocked on, so it has to be killed rather than waited out.
    #[tokio::test]
    async fn exec_kills_the_process_when_waiting_fails() {
        let daemon = FakeExecDaemon::failing_wait();
        let err = bounded_exec(&daemon)
            .await
            .expect_err("a failed wait must surface as an error");

        assert!(
            format!("{err}").contains("exec wait failed"),
            "the error must name the failed step, got: {err}"
        );
        assert!(
            daemon.calls().contains(&"kill"),
            "a failed wait must stop the process, got: {:?}",
            daemon.calls()
        );
    }

    /// A start failure must not be reported as a successful command, and it
    /// leaves an exit wait already registered with the daemon: the process has
    /// to be torn down so that wait is answered rather than outstanding
    /// forever.
    #[tokio::test]
    async fn exec_surfaces_a_start_failure_and_stops_the_process() {
        let daemon = FakeExecDaemon::failing_start();
        let err = bounded_exec(&daemon)
            .await
            .expect_err("a failed start must surface as an error");

        assert!(
            format!("{err}").contains("exec start failed"),
            "the error must name the failed step, got: {err}"
        );
        assert!(
            daemon.calls().contains(&"kill"),
            "a failed start must stop the process, got: {:?}",
            daemon.calls()
        );
    }

    // ---- issue #4 regression coverage: an undecodable containerList entry
    // must not read as "this workspace has no container" ----

    fn undecodable(id: &str, local_folder: &str) -> UndecodableSnapshot {
        UndecodableSnapshot {
            id: id.to_string(),
            labels: HashMap::from([(
                "devcontainer.local_folder".to_string(),
                local_folder.to_string(),
            )]),
            error: "missing field `status`".to_string(),
        }
    }

    /// An entry the models cannot read that carries *this* workspace's labels,
    /// with no readable container to fall back on, must fail the query.
    /// Silently dropping it makes `dev up` see an empty list and create a
    /// duplicate the daemon then rejects with an error that says nothing about
    /// the real cause.
    #[test]
    fn a_query_that_matches_only_an_undecodable_entry_fails_with_the_decode_cause() {
        let entries = vec![undecodable("vsc-mine", "/w/mine")];
        let filters = [("devcontainer.local_folder", "/w/mine")];

        let err = report_undecodable_match(&entries, &filters, 0)
            .expect_err("the workspace's own unreadable container must be reported");
        let message = format!("{err}");
        assert!(message.contains("vsc-mine"), "{message}");
        assert!(message.contains("missing field `status`"), "{message}");
    }

    /// The hard error exists only to stop `dev up` creating a duplicate it
    /// cannot see. Once the query *did* find a readable container for these
    /// labels, every command has what it came for — failing anyway would deny
    /// `dev status`/`exec`/`shell` a container they can reach and leave
    /// `dev down` unable to clean up.
    #[test]
    fn a_readable_match_keeps_the_query_working_despite_an_undecodable_sibling() {
        let entries = vec![undecodable("vsc-mine-stale", "/w/mine")];
        let filters = [("devcontainer.local_folder", "/w/mine")];

        assert!(
            report_undecodable_match(&entries, &filters, 1).is_ok(),
            "a reachable container must not be hidden by an unreadable sibling"
        );
    }

    /// The tolerance that made the fix work stays: another workspace's
    /// unreadable container must not break this workspace's discovery.
    #[test]
    fn an_unrelated_undecodable_entry_does_not_fail_the_query() {
        let entries = vec![undecodable("vsc-theirs", "/w/theirs")];
        assert!(
            report_undecodable_match(&entries, &[("devcontainer.local_folder", "/w/mine")], 0)
                .is_ok()
        );
        // Nor may an entry whose labels are unreadable too be blamed on a
        // workspace that never owned it.
        let bare = vec![UndecodableSnapshot {
            id: "<unknown>".to_string(),
            labels: HashMap::new(),
            error: "unreadable".to_string(),
        }];
        assert!(
            report_undecodable_match(&bare, &[("devcontainer.local_folder", "/w/mine")], 0).is_ok()
        );
    }

    /// An unfiltered listing asks about every container rather than a specific
    /// one, so it keeps reporting the containers it *can* read.
    #[test]
    fn an_unfiltered_listing_still_tolerates_undecodable_entries() {
        let entries = vec![undecodable("vsc-mine", "/w/mine")];
        assert!(report_undecodable_match(&entries, &[], 0).is_ok());
    }

    /// `remoteUser` is normally a name, and a name must not silently become
    /// root: `dev exec --user vscode` and every lifecycle hook would otherwise
    /// run as uid 0 on this runtime but as the named user on docker/podman.
    #[test]
    fn named_user_maps_to_raw_user_string() {
        match to_apple_user(Some("vscode")) {
            User::Raw { raw } => assert_eq!(raw.user_string, "vscode"),
            other => panic!("named user must map to User::Raw, got {other:?}"),
        }
        match to_apple_user(Some("node:node")) {
            User::Raw { raw } => assert_eq!(raw.user_string, "node:node"),
            other => panic!("named user:group must map to User::Raw, got {other:?}"),
        }
    }

    /// Numeric specs still take the id path, including the `uid:gid` form.
    #[test]
    fn numeric_user_maps_to_ids() {
        match to_apple_user(Some("1000")) {
            User::Id { id } => assert_eq!((id.uid, id.gid), (1000, 1000)),
            other => panic!("numeric user must map to User::Id, got {other:?}"),
        }
        match to_apple_user(Some("1000:2000")) {
            User::Id { id } => assert_eq!((id.uid, id.gid), (1000, 2000)),
            other => panic!("numeric uid:gid must map to User::Id, got {other:?}"),
        }
    }

    /// No user (and no meaningful user) means root, as before.
    #[test]
    fn missing_user_defaults_to_root_ids() {
        for spec in [None, Some(""), Some("   ")] {
            match to_apple_user(spec) {
                User::Id { id } => assert_eq!((id.uid, id.gid), (0, 0)),
                other => panic!("{spec:?} must default to uid/gid 0, got {other:?}"),
            }
        }
    }

    /// The daemon keys processes by identifier within a container, and `dev up`
    /// execs lifecycle hooks concurrently — so two execs from one CLI run must
    /// never share an identifier.
    #[test]
    fn exec_process_ids_are_unique_per_call() {
        let ids: Vec<String> = (0..64).map(|_| next_process_id("exec")).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "exec process ids must be unique");
        assert!(ids.iter().all(|id| id.starts_with("exec-")));
    }

    /// The command's first word is the executable and the rest are arguments;
    /// an empty command must not panic.
    #[test]
    fn exec_process_config_splits_command() {
        let config = exec_process_config(
            &["sh".to_string(), "-c".to_string(), "echo hi".to_string()],
            None,
            false,
            "/workspaces/demo",
        );
        assert_eq!(config.executable, "sh");
        assert_eq!(config.arguments, vec!["-c", "echo hi"]);
        assert!(!config.terminal);
        assert_eq!(config.working_directory, "/workspaces/demo");

        let empty = exec_process_config(&[], None, true, "/");
        assert_eq!(empty.executable, "");
        assert!(empty.arguments.is_empty());
        assert!(empty.terminal);
    }

    /// An exec must start in the container's own working directory, the way
    /// `docker exec` inherits the container's `WorkingDir`. Running everything
    /// from `/` breaks relative-path lifecycle hooks such as
    /// `postCreateCommand: npm install`.
    #[test]
    fn exec_working_directory_is_not_forced_to_root() {
        let config = exec_process_config(
            &["npm".to_string(), "install".to_string()],
            None,
            false,
            "/workspaces/project",
        );
        assert_eq!(
            config.working_directory, "/workspaces/project",
            "exec must run in the container's working directory"
        );
    }

    /// The daemon chdirs into whatever string it is handed, so an unusable
    /// value has to become `/` rather than fail the process.
    #[test]
    fn unusable_working_directories_fall_back_to_root() {
        for spec in ["", "relative/path"] {
            let config = exec_process_config(&["sh".to_string()], None, false, spec);
            assert_eq!(
                config.working_directory, "/",
                "{spec:?} must fall back to /"
            );
        }
    }

    /// The resolved `workspaceFolder` is where a devcontainer's commands
    /// belong, so it wins over both the mount root and the image's
    /// `WorkingDir`; each of those is in turn the fallback for a container that
    /// has no folder, and no workspace at all.
    #[test]
    fn container_working_directory_prefers_the_workspace_folder() {
        let mount = WorkspaceMount {
            source: std::path::PathBuf::from("/host/monorepo"),
            target: "/srv/app".to_string(),
        };

        assert_eq!(
            container_working_directory(
                Some("/srv/app/packages/api"),
                Some(&mount),
                Some("/usr/src/app")
            ),
            "/srv/app/packages/api",
            "a workspaceFolder subdirectory must win over the mount root"
        );
        assert_eq!(
            container_working_directory(None, Some(&mount), Some("/usr/src/app")),
            "/srv/app"
        );
        assert_eq!(
            container_working_directory(None, None, Some("/usr/src/app")),
            "/usr/src/app"
        );
        assert_eq!(container_working_directory(None, None, None), "/");
        assert_eq!(container_working_directory(Some(""), None, Some("")), "/");
        assert_eq!(
            container_working_directory(Some("packages/api"), None, None),
            "/"
        );
    }

    /// The container config `dev up` produces for a monorepo must start its
    /// processes in the configured project directory, which is what every exec
    /// then inherits.
    #[test]
    fn to_apple_config_runs_in_the_resolved_workspace_folder() {
        let mut container_config = minimal_container_config();
        container_config.workspace_mount = Some(WorkspaceMount {
            source: std::path::PathBuf::from("/host/monorepo"),
            target: "/srv/app".to_string(),
        });
        container_config.workspace_folder = Some("/srv/app/packages/api".to_string());

        let apple_config = to_apple_config(
            &container_config,
            ImageDescription::default(),
            &CachedImageConfig {
                working_dir: Some("/usr/src/app".to_string()),
                ..CachedImageConfig::default()
            },
        );

        assert_eq!(
            apple_config.init_process.working_directory,
            "/srv/app/packages/api"
        );
        assert_eq!(
            apple_config.mounts.last().map(|m| m.destination.as_str()),
            Some("/srv/app"),
            "the source tree is still mounted at the mount target"
        );
    }

    fn minimal_container_config() -> ContainerConfig {
        ContainerConfig {
            image: "docker.io/library/alpine:latest".to_string(),
            name: "vsc-test".to_string(),
            labels: HashMap::new(),
            env: HashMap::new(),
            mounts: vec![],
            volumes: vec![],
            ports: vec![],
            workspace_mount: None,
            workspace_folder: None,
            extra_args: vec![],
            entrypoint: None,
            init: false,
            privileged: false,
            cap_add: vec![],
            security_opt: vec![],
        }
    }

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

    /// A locally built image has no registry to ask, so its environment and
    /// working directory can only come from the config blob on disk. Losing
    /// them silently drops whatever the devcontainer features put on `PATH`.
    #[test]
    fn image_config_is_read_from_an_oci_config_document() {
        let document = serde_json::json!({
            "config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin", "NODE_VERSION=22"],
                "User": "node",
                "WorkingDir": "/workspaces"
            }
        });

        let config = CachedImageConfig::from_oci_config(&document);
        assert_eq!(
            config.env,
            vec![
                "PATH=/usr/local/bin:/usr/bin".to_string(),
                "NODE_VERSION=22".to_string()
            ]
        );
        assert_eq!(config.user.as_deref(), Some("node"));
        assert_eq!(config.working_dir.as_deref(), Some("/workspaces"));
    }

    #[test]
    fn an_image_config_without_a_config_section_yields_defaults() {
        let config = CachedImageConfig::from_oci_config(&serde_json::json!({}));
        assert!(config.env.is_empty());
        assert_eq!(config.user, None);
        assert_eq!(config.working_dir, None);
    }

    /// Two workspaces whose names share a long prefix must not collide. The retained
    /// suffix is what supplies the entropy, so a prefix-only truncation would be wrong.
    #[test]
    fn truncation_preserves_distinguishing_suffix() {
        let a = format!("vsc-my-project-with-a-long-name-{}", "1".repeat(64));
        let b = format!("vsc-my-project-with-a-long-name-{}", "2".repeat(64));

        let (id_a, id_b) = (truncate_container_id(&a), truncate_container_id(&b));

        assert_ne!(
            id_a, id_b,
            "names differing only in suffix must not collide"
        );
        assert!(id_a.ends_with(&a[a.len() - 18..]));
    }

    /// The Apple container config built from a `dev up`-style `ContainerConfig`
    /// must (a) truncate the ID to Apple's limit and (b) carry the
    /// `devcontainer.local_folder` label that `dev status`/`dev exec` filter on.
    ///
    /// This characterizes the creation→discovery contract at the heart of issue
    /// #4 — `dev up` reported readiness for a container `dev status`/`dev exec`
    /// could not find. It does not reproduce the original break (which was a
    /// `containerList` decode failure, covered in
    /// `apple_container::models::tests`); it pins the two invariants any future
    /// change to `to_apple_config` must preserve: the ID stays inside the
    /// daemon's length limit, and the labels `workspace_labels(workspace, None)`
    /// queries with are actually set on the created container.
    #[test]
    fn to_apple_config_truncates_id_and_carries_discovery_label() {
        use crate::runtime::WorkspaceMount;
        use crate::util::{container_name, workspace_labels};

        let workspace = std::path::Path::new("/tmp/test-apple-repro");
        let config_file = workspace.join(".devcontainer/devcontainer.json");

        // Build the ContainerConfig exactly as `commands::up::run` does: the
        // name comes from `container_name(workspace)` and the labels from
        // `workspace_labels(workspace, Some(config_file))`.
        let name = container_name(workspace);
        let labels: HashMap<String, String> = workspace_labels(workspace, Some(&config_file))
            .into_iter()
            .collect();
        let local_folder = labels
            .get("devcontainer.local_folder")
            .expect("up config must set devcontainer.local_folder")
            .clone();

        let container_config = ContainerConfig {
            image: "docker.io/library/alpine:latest".to_string(),
            name: name.clone(),
            labels,
            env: HashMap::new(),
            mounts: vec![],
            volumes: vec![],
            ports: vec![],
            workspace_mount: Some(WorkspaceMount {
                source: workspace.to_path_buf(),
                target: "/workspaces/test-apple-repro".to_string(),
            }),
            workspace_folder: Some("/workspaces/test-apple-repro".to_string()),
            extra_args: vec![],
            entrypoint: None,
            init: false,
            privileged: false,
            cap_add: vec![],
            security_opt: vec![],
        };

        let image = ImageDescription::default();
        let image_config = CachedImageConfig {
            working_dir: Some("/usr/src/app".to_string()),
            ..CachedImageConfig::default()
        };
        let apple_config = to_apple_config(&container_config, image, &image_config);

        // (a) ID fits Apple's daemon limit.
        assert_eq!(
            apple_config.id.len(),
            MAX_CONTAINER_ID_LEN,
            "Apple container ID must be truncated to the daemon limit"
        );
        assert_ne!(apple_config.id, container_config.name);

        // (b) The discovery label survives into the Apple config, and the label
        // that `dev status`/`dev exec` query with (`workspace_labels(workspace, None)`,
        // which is local_folder only) matches it — so a container created by
        // `dev up` is findable by the same workspace label.
        assert_eq!(
            apple_config.labels.get("devcontainer.local_folder"),
            Some(&local_folder),
            "created container must carry the workspace local_folder label"
        );
        let discovery_labels = workspace_labels(workspace, None);
        for (key, value) in &discovery_labels {
            assert_eq!(
                apple_config.labels.get(key),
                Some(value),
                "discovery label {key} must match the label set at create time"
            );
        }

        // (c) The container runs in the workspace folder, which is what execs
        // inherit — lifecycle hooks would otherwise run in `/`.
        assert_eq!(
            apple_config.init_process.working_directory, "/workspaces/test-apple-repro",
            "container working directory must be the workspace folder"
        );
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
        let image: ImageDescription =
            serde_json::from_slice(&image_desc_bytes).expect("parse image descriptor failed");

        // Get default kernel
        let kernel = runtime
            .client
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
                environment: vec![
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                ],
                working_directory: "/".to_string(),
                terminal: false,
                user: User::Raw {
                    raw: UserString {
                        user_string: "root".to_string(),
                    },
                },
                supplemental_groups: vec![],
                rlimits: vec![],
            },
            resources: Resources {
                cpus: 4,
                memory_in_bytes: 1024 * 1024 * 1024,
            },
            runtime_handler: "container-runtime-linux".to_string(),
            platform: Platform {
                architecture: "arm64".to_string(),
                os: "linux".to_string(),
            },
            networks: vec![NetworkInfo {
                network: "default".to_string(),
                options: NetworkOptions {
                    hostname: Some(container_id.to_string()),
                    mtu: Some(1280),
                },
            }],
            dns: Some(DnsInfo {
                nameservers: vec![],
                search_domains: vec![],
                options: vec![],
            }),
        };

        // Clean up any previous test container
        let _ = runtime.client.stop(container_id).await;
        let _ = runtime.client.delete(container_id, true).await;

        // Create
        runtime
            .client
            .create(&config, &kernel)
            .await
            .expect("create failed");

        // Start (bootstrap + start_process)
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = devnull.as_raw_fd();
        runtime
            .client
            .bootstrap(container_id, fd, fd, fd)
            .await
            .expect("bootstrap failed");
        runtime
            .client
            .start_process(container_id, container_id)
            .await
            .expect("start_process failed");

        // Verify running
        let snapshot = runtime.client.get(container_id).await.expect("get failed");
        assert_eq!(
            snapshot.status,
            RuntimeStatus::Running,
            "container should be running"
        );

        // Clean up
        runtime
            .client
            .stop(container_id)
            .await
            .expect("stop failed");
        runtime
            .client
            .delete(container_id, true)
            .await
            .expect("delete failed");
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
        let truncated_id = format!(
            "{}-{}",
            &container_id[..17],
            &container_id[container_id.len() - 18..]
        );
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
            r#"{"name":"Test","image":"mcr.microsoft.com/devcontainers/base:ubuntu"}"#,
        )
        .unwrap();

        // Build a ContainerConfig matching what dev up would create
        let container_config = ContainerConfig {
            image: image_ref.to_string(),
            name: container_id.to_string(),
            labels: {
                let mut labels = HashMap::new();
                labels.insert(
                    "devcontainer.local_folder".to_string(),
                    workspace_path.to_string_lossy().to_string(),
                );
                labels.insert(
                    "devcontainer.config_file".to_string(),
                    devcontainer_dir
                        .join("devcontainer.json")
                        .to_string_lossy()
                        .to_string(),
                );
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
            workspace_folder: Some("/workspaces/test-apple-workspace".to_string()),
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
        let id = runtime
            .create_container(&container_config)
            .await
            .expect("create_container failed");
        assert_eq!(
            id, truncated_id,
            "create_container should return truncated ID"
        );
        runtime
            .start_container(&id)
            .await
            .expect("start_container failed");

        // Verify running
        let snapshot = runtime.client.get(&id).await.expect("get failed");
        assert_eq!(
            snapshot.status,
            RuntimeStatus::Running,
            "container should be running"
        );

        // Clean up
        runtime
            .client
            .stop(&truncated_id)
            .await
            .expect("stop failed");
        runtime
            .client
            .delete(&truncated_id, true)
            .await
            .expect("delete failed");
    }

    /// Integration test for the `dev exec` path. Covers the three ways exec was
    /// broken on the issue #4 path: the process was never started (so `exec`
    /// hung forever), the exit code was hardcoded to 0 (so every failure looked
    /// like success), and stdin was never closed (so any command that reads it
    /// hung forever).
    ///
    /// Ignored by default: requires a running Apple Container daemon and pulls
    /// an image over the network. Run with
    /// `cargo test --features apple -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a live Apple Container daemon and network access"]
    #[cfg(target_os = "macos")]
    async fn test_apple_runtime_exec_runs_and_returns() {
        let runtime = AppleRuntime::connect().expect("connect failed");
        let container_id = "apple-runtime-exec-test";
        let image_ref = "docker.io/library/alpine:latest";

        // Pull and unpack image.
        let platform = serde_json::json!({"architecture": "arm64", "os": "linux"});
        let platform_json = serde_json::to_vec(&platform).unwrap();
        let image_desc_bytes = apple_container::build::pull_image(image_ref, &platform_json)
            .await
            .expect("pull_image failed");
        apple_container::build::unpack_image(&image_desc_bytes, &platform_json)
            .await
            .expect("unpack_image failed");
        let image: ImageDescription =
            serde_json::from_slice(&image_desc_bytes).expect("parse image descriptor failed");
        let kernel = runtime
            .client
            .get_default_kernel()
            .await
            .expect("get_default_kernel failed");

        let config = ContainerConfiguration {
            id: container_id.to_string(),
            image,
            mounts: vec![],
            published_ports: vec![],
            labels: HashMap::new(),
            init_process: ProcessConfiguration {
                executable: "sleep".to_string(),
                arguments: vec!["3600".to_string()],
                environment: vec![],
                working_directory: "/tmp".to_string(),
                terminal: false,
                user: User::Raw {
                    raw: UserString {
                        user_string: "root".to_string(),
                    },
                },
                supplemental_groups: vec![],
                rlimits: vec![],
            },
            resources: Resources {
                cpus: 4,
                memory_in_bytes: 1024 * 1024 * 1024,
            },
            runtime_handler: "container-runtime-linux".to_string(),
            platform: Platform {
                architecture: "arm64".to_string(),
                os: "linux".to_string(),
            },
            networks: vec![NetworkInfo {
                network: "default".to_string(),
                options: NetworkOptions {
                    hostname: Some(container_id.to_string()),
                    mtu: Some(1280),
                },
            }],
            dns: Some(DnsInfo {
                nameservers: vec![],
                search_domains: vec![],
                options: vec![],
            }),
        };

        // Clean up any previous test container.
        let _ = runtime.client.stop(container_id).await;
        let _ = runtime.client.delete(container_id, true).await;

        runtime
            .client
            .create(&config, &kernel)
            .await
            .expect("create failed");
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = devnull.as_raw_fd();
        runtime
            .client
            .bootstrap(container_id, fd, fd, fd)
            .await
            .expect("bootstrap failed");
        runtime
            .client
            .start_process(container_id, container_id)
            .await
            .expect("start_process failed");

        // Each exec is wrapped in a timeout so a regression that reintroduces a
        // hang fails fast instead of stalling CI.
        async fn bounded(runtime: &AppleRuntime, id: &str, cmd: &[&str]) -> ExecResult {
            let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
            let described = cmd.join(" ");
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                runtime.exec(id, &cmd, None),
            )
            .await
            .unwrap_or_else(|_| panic!("exec hung: {described}"))
            .unwrap_or_else(|e| panic!("exec failed: {described}: {e}"))
        }

        let result = bounded(&runtime, container_id, &["echo", "hello"]).await;
        assert_eq!(
            result.stdout.trim(),
            "hello",
            "exec should return command output"
        );
        assert_eq!(result.exit_code, 0, "a successful command must report 0");

        // A failing command must report its real status, or lifecycle hook
        // failures and `dev exec` exit codes are silently swallowed.
        let failed = bounded(
            &runtime,
            container_id,
            &["sh", "-c", "echo oops >&2; exit 7"],
        )
        .await;
        assert_eq!(failed.exit_code, 7, "exit code must be propagated");
        assert_eq!(failed.stderr.trim(), "oops");

        // An exec starts in the container's working directory, not `/`, so
        // relative-path lifecycle hooks resolve the way they do on docker.
        let cwd = bounded(&runtime, container_id, &["pwd"]).await;
        assert_eq!(
            cwd.stdout.trim(),
            "/tmp",
            "exec must inherit the container's working directory"
        );

        // Nothing writes to the exec'd process's stdin, so it must see EOF.
        // `cat` would otherwise block until the container is killed.
        let piped = bounded(&runtime, container_id, &["cat"]).await;
        assert_eq!(piped.exit_code, 0, "cat must exit once stdin reaches EOF");
        assert!(piped.stdout.is_empty());

        // Clean up.
        runtime
            .client
            .stop(container_id)
            .await
            .expect("stop failed");
        runtime
            .client
            .delete(container_id, true)
            .await
            .expect("delete failed");
    }
}
