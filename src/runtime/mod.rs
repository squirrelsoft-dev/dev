#[cfg(target_os = "macos")]
pub mod apple;
pub mod docker;
pub mod podman;

use crate::error::DevError;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

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
    pub ports: Vec<PortMapping>,
    pub workspace_mount: Option<WorkspaceMount>,
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
}

/// A boxed future that is Send.
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, DevError>> + Send + 'a>>;

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

    fn exec_interactive(&self, id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ()>;

    fn inspect_container(&self, id: &str) -> BoxFut<'_, ContainerInfo>;

    fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>>;

    fn image_exists(&self, image: &str) -> BoxFut<'_, bool>;

    fn inspect_image_metadata(&self, image: &str) -> BoxFut<'_, ImageMetadata>;
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
            #[cfg(target_os = "macos")]
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
        if let Ok(rt) = docker::DockerRuntime::connect() {
            if rt.ping().await.is_ok() {
                found = Some(rt);
            }
        }
        // Fallback: Docker Desktop on macOS uses ~/.docker/run/docker.sock
        // while /var/run/docker.sock may point to a different runtime.
        if found.is_none() {
            if let Some(rt) = docker::DockerRuntime::connect_fallback() {
                if rt.ping().await.is_ok() {
                    found = Some(rt);
                }
            }
        }
        found
    };

    let podman_running = if let Ok(rt) = podman::PodmanRuntime::connect() {
        if rt.ping().await.is_ok() { Some(rt) } else { None }
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
        (None, None) => DevError::NoRuntime(
            "No container runtime found. Install Docker or Podman.".to_string(),
        ),
    }
}
