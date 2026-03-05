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
    pub extra_args: Vec<String>,
    pub entrypoint: Option<String>,
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

/// A boxed future that is Send.
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, DevError>> + Send + 'a>>;

/// Trait abstracting over container runtimes (Docker, Podman, Apple Containers).
#[allow(dead_code)]
pub trait ContainerRuntime: Send + Sync {
    fn pull_image(&self, image: &str) -> BoxFut<'_, ()>;

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
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

    fn list_containers(&self, label_filter: &str) -> BoxFut<'_, Vec<ContainerInfo>>;
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
            "apple" => {
                let rt = apple::AppleRuntime::connect()?;
                rt.ping().await?;
                Ok(Box::new(rt))
            }
            other => Err(DevError::Runtime(format!("Unknown runtime: {other}"))),
        };
    }

    // Auto-detect: try Apple Containers first (macOS-native), then Docker, then Podman.
    if cfg!(target_os = "macos") {
        if let Ok(rt) = apple::AppleRuntime::connect() {
            if rt.ping().await.is_ok() {
                return Ok(Box::new(rt));
            }
        }
    }

    if let Ok(rt) = docker::DockerRuntime::connect() {
        if rt.ping().await.is_ok() {
            return Ok(Box::new(rt));
        }
    }

    if let Ok(rt) = podman::PodmanRuntime::connect() {
        if rt.ping().await.is_ok() {
            return Ok(Box::new(rt));
        }
    }

    Err(diagnose_no_runtime().await)
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

/// Like run_command but returns combined stdout+stderr regardless of exit code.
async fn run_command_any(cmd: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Some(combined)
}

async fn diagnose_no_runtime() -> DevError {
    // Check Apple Containers
    if command_exists("container").await {
        let container_not_running = run_command_any("container", &["system", "status"])
            .await
            .map(|s| s.to_lowercase().contains("not running"))
            .unwrap_or(true);

        if container_not_running {
            return DevError::NoRuntime(
                "Apple Containers is installed but not running.\n\nRun:\n  container system start\n\nThen try `dev <subcommand>` again.".to_string(),
            );
        }
    }

    // Check Podman
    if command_exists("podman").await {
        if let Some(json) = run_command("podman", &["machine", "list", "--format", "json"]).await {
            if let Ok(machines) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(arr) = machines.as_array() {
                    if arr.is_empty() {
                        return DevError::NoRuntime(
                            "Podman is installed but no machine exists.\n\nRun:\n  podman machine init && podman machine start\n\nThen try `dev <subcommand>` again.".to_string(),
                        );
                    }
                    let any_running = arr.iter().any(|m| {
                        m.get("Running").and_then(|r| r.as_bool()).unwrap_or(false)
                    });
                    if !any_running {
                        return DevError::NoRuntime(
                            "Podman is installed but no machine is running.\n\nRun:\n  podman machine start\n\nThen try `dev <subcommand>` again.".to_string(),
                        );
                    }
                }
            }
        }
    }

    // Check Docker
    if command_exists("docker").await {
        return DevError::NoRuntime(
            "Docker is installed but the daemon is not running.\n\nStart Docker Desktop or the Docker daemon, then try `dev <subcommand>` again.".to_string(),
        );
    }

    // Nothing installed
    DevError::NoRuntime(
        "No container runtime found. Install Docker, Podman, or Apple Containers.".to_string(),
    )
}
