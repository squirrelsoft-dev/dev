use std::path::Path;

use crate::error::DevError;
use crate::runtime::docker::BollardRuntime;
use crate::runtime::{
    BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ExecResult,
};

/// Podman runtime backed by the same bollard client, connecting to the Podman socket.
pub struct PodmanRuntime(pub(crate) BollardRuntime);

impl PodmanRuntime {
    pub fn connect() -> Result<Self, DevError> {
        let socket = podman_socket_path()?;
        Ok(Self(BollardRuntime::connect_to_socket(&socket)?))
    }

    pub async fn ping(&self) -> Result<(), DevError> {
        self.0.ping().await
    }
}

fn podman_socket_path() -> Result<String, DevError> {
    // Prefer $XDG_RUNTIME_DIR/podman/podman.sock, fall back to common locations.
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let path = format!("{xdg}/podman/podman.sock");
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }

    // macOS via Homebrew podman machine
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            let path = format!("{home}/.local/share/containers/podman/machine/podman.sock");
            if std::path::Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    // Linux fallback
    let uid_path = format!("/run/user/{}/podman/podman.sock", unsafe {
        libc::getuid()
    });
    if std::path::Path::new(&uid_path).exists() {
        return Ok(uid_path);
    }

    Err(DevError::Runtime(
        "Could not find Podman socket".to_string(),
    ))
}

impl ContainerRuntime for PodmanRuntime {
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
