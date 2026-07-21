#[cfg(all(target_os = "macos", feature = "apple"))]
pub mod apple;
pub mod compose;
pub mod docker;
pub mod podman;

use crate::error::DevError;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

/// Container state as reported by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ContainerState {
    Running,
    Stopped,
    NotFound,
}

/// Configuration for creating a new container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub image: String,
    pub name: String,
    pub labels: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub mounts: Vec<BindMount>,
    pub volumes: Vec<VolumeMount>,
    pub ports: Vec<PortMapping>,
    pub workspace_mount: Option<WorkspaceMount>,
    /// Resolved `workspaceFolder`: where commands run inside the container.
    /// Equal to the workspace mount target unless the config selects a
    /// subdirectory of it.
    pub workspace_folder: Option<String>,
    #[allow(dead_code)]
    pub extra_args: Vec<String>,
    pub entrypoint: Option<String>,
    /// Run an init process inside the container (--init).
    pub init: bool,
    /// Run the container in privileged mode (--privileged).
    pub privileged: bool,
    /// Additional Linux capabilities to add (--cap-add).
    pub cap_add: Vec<String>,
    /// Security options (--security-opt).
    pub security_opt: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: String,
    pub readonly: bool,
}

/// A named Docker volume mounted into the container.
#[derive(Debug, Clone)]
pub struct VolumeMount {
    pub name: String,
    pub target: String,
    pub readonly: bool,
}

#[derive(Debug, Clone)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
}

#[derive(Debug, Clone)]
pub struct WorkspaceMount {
    pub source: PathBuf,
    pub target: String,
}

/// Metadata about an existing container.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub labels: HashMap<String, String>,
    pub image: String,
}

/// Result of a non-interactive exec command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Metadata extracted from a container image's labels and config.
#[derive(Debug, Clone, Default)]
pub struct ImageMetadata {
    pub remote_user: Option<String>,
    pub container_user: Option<String>,
    /// Raw `devcontainer.metadata` label entries, in label order. Empty when the label
    /// is absent or unparseable. Retained so callers can recover settings contributed by
    /// the features that built an image without re-resolving those features.
    pub metadata_entries: Vec<serde_json::Value>,
    /// Environment variables from the OCI image config (`Env` field).
    #[allow(dead_code)]
    pub env: Vec<String>,
}

/// Handle to a running exec session with attached stdin/stdout byte streams.
pub struct AttachedExec {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
}

/// A boxed future that is Send.
pub(crate) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, DevError>> + Send + 'a>>;

/// Current terminal size as (columns, rows), or None when stdout is not a tty.
pub(crate) fn terminal_size() -> Option<(u16, u16)> {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdout().as_raw_fd();
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 && ws.ws_row > 0
    {
        Some((ws.ws_col, ws.ws_row))
    } else {
        None
    }
}

/// Trait abstracting over container runtimes (Docker, Podman, Apple Containers).
#[allow(dead_code)]
pub trait ContainerRuntime: Send + Sync {
    /// Short name identifying this runtime (e.g. "docker", "podman", "apple").
    fn runtime_name(&self) -> &'static str;

    fn pull_image(&self, image: &str) -> BoxFut<'_, ()>;

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        build_args: &HashMap<String, String>,
        no_cache: bool,
        verbose: bool,
    ) -> BoxFut<'_, ()>;

    fn create_container(&self, config: &ContainerConfig) -> BoxFut<'_, String>;

    fn start_container(&self, id: &str) -> BoxFut<'_, ()>;

    fn stop_container(&self, id: &str) -> BoxFut<'_, ()>;

    fn remove_container(&self, id: &str) -> BoxFut<'_, ()>;

    fn exec(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ExecResult>;

    /// Whether an [`Self::exec`] failure means the image has no such
    /// executable, rather than the runtime being unable to run one at all.
    ///
    /// Only the runtime knows the difference, because only it knows which of
    /// its own failures can carry that meaning: docker declines to start the
    /// exec and answers with a server error, while Apple's daemon fails the
    /// start step. Callers use this to tell an image without a shell — which is
    /// the image's business — from a container they cannot run anything in.
    ///
    /// A process that ran and exited is not this: whatever status it reported,
    /// the runtime created, started and waited for it. Implementations must
    /// answer `false` for anything they cannot attribute to the command itself,
    /// so a transport, start or wait failure stays fatal to readiness.
    fn exec_reports_missing_command(&self, _error: &DevError) -> bool {
        false
    }

    /// Run a command attached to the caller's terminal, returning its exit code
    /// once it finishes.
    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, i32>;

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo>;

    fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>>;

    fn image_exists(&self, image: &str) -> BoxFut<'_, bool>;

    fn inspect_image_metadata(&self, image: &str) -> BoxFut<'_, ImageMetadata>;

    /// Create an exec session with attached stdin/stdout streams (no TTY).
    /// Used for port forwarding via netcat inside the container.
    fn exec_attached(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
    ) -> BoxFut<'_, AttachedExec>;
}

/// Resolve the effective remote user by checking the devcontainer config first,
/// then falling back to the image's embedded metadata.
pub async fn resolve_remote_user(
    runtime: &dyn ContainerRuntime,
    image: &str,
    config_user: Option<&str>,
) -> Result<Option<String>, DevError> {
    if let Some(u) = config_user {
        return Ok(Some(u.to_string()));
    }
    let meta = runtime.inspect_image_metadata(image).await?;
    Ok(meta.remote_user.or(meta.container_user))
}

/// Detect which container runtime is available, or use an explicit override.
pub async fn detect_runtime(
    override_runtime: Option<&str>,
) -> Result<Box<dyn ContainerRuntime>, DevError> {
    if let Some(name) = override_runtime {
        return match name {
            "docker" => {
                let rt = docker::DockerRuntime::connect()?;
                Ok(Box::new(rt))
            }
            "podman" => {
                let rt = podman::PodmanRuntime::connect()?;
                Ok(Box::new(rt))
            }
            #[cfg(all(target_os = "macos", feature = "apple"))]
            "apple" => {
                let rt = apple::AppleRuntime::connect()?;
                rt.ping().await?;
                Ok(Box::new(rt))
            }
            other => Err(DevError::Runtime(format!("Unknown runtime: {other}"))),
        };
    }

    // Auto-detect: check which runtimes are actually running.
    // Apple Containers disabled for now — use --runtime apple to opt in.
    let docker_running = {
        let mut found = None;
        // Try default socket (DOCKER_HOST or /var/run/docker.sock)
        if let Ok(rt) = docker::DockerRuntime::connect()
            && rt.ping().await.is_ok()
        {
            found = Some(rt);
        }
        // Fallback: Docker Desktop on macOS uses ~/.docker/run/docker.sock
        // while /var/run/docker.sock may point to a different runtime.
        if found.is_none()
            && let Some(rt) = docker::DockerRuntime::connect_fallback()
            && rt.ping().await.is_ok()
        {
            found = Some(rt);
        }
        found
    };

    let podman_running = if let Ok(rt) = podman::PodmanRuntime::connect() {
        if rt.ping().await.is_ok() {
            Some(rt)
        } else {
            None
        }
    } else {
        None
    };

    // Prefer Podman if both are running, otherwise use whichever is running.
    match (podman_running, docker_running) {
        (Some(rt), _) => Ok(Box::new(rt)),
        (None, Some(rt)) => Ok(Box::new(rt)),
        (None, None) => Err(diagnose_no_runtime().await),
    }
}

async fn command_exists(cmd: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(cmd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}
async fn diagnose_no_runtime() -> DevError {
    // Apple Containers diagnostic disabled for now.
    let has_docker = command_exists("docker").await;
    let has_podman = command_exists("podman").await;

    let podman_hint = if has_podman {
        if let Some(json) = run_command("podman", &["machine", "list", "--format", "json"]).await {
            if let Ok(machines) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(arr) = machines.as_array() {
                    if arr.is_empty() {
                        Some("  podman machine init && podman machine start")
                    } else {
                        Some("  podman machine start")
                    }
                } else {
                    Some("  podman machine start")
                }
            } else {
                Some("  podman machine start")
            }
        } else {
            Some("  podman machine start")
        }
    } else {
        None
    };

    let docker_hint = if has_docker {
        Some("  Start Docker Desktop (or the Docker daemon)")
    } else {
        None
    };

    match (podman_hint, docker_hint) {
        (Some(podman), Some(docker)) => DevError::NoRuntime(format!(
            "Both Podman and Docker are installed but neither is running.\n\nStart one of them:\n{podman}\n  — or —\n{docker}\n\nThen try `dev <subcommand>` again."
        )),
        (Some(podman), None) => DevError::NoRuntime(format!(
            "Podman is installed but not running.\n\nRun:\n{podman}\n\nThen try `dev <subcommand>` again."
        )),
        (None, Some(docker)) => DevError::NoRuntime(format!(
            "Docker is installed but the daemon is not running.\n\n{docker}, then try `dev <subcommand>` again."
        )),
        (None, None) => {
            DevError::NoRuntime("No container runtime found. Install Docker or Podman.".to_string())
        }
    }
}
