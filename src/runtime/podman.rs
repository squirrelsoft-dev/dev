use std::path::Path;

use crate::error::DevError;
use crate::runtime::docker::BollardRuntime;
use crate::runtime::{
    AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ExecResult,
    ImageMetadata,
};
use std::os::unix::process::CommandExt;

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
    if cfg!(target_os = "macos")
        && let Ok(home) = std::env::var("HOME")
    {
        let path = format!("{home}/.local/share/containers/podman/machine/podman.sock");
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }

    // Linux fallback
    let uid_path = format!("/run/user/{}/podman/podman.sock", unsafe { libc::getuid() });
    if std::path::Path::new(&uid_path).exists() {
        return Ok(uid_path);
    }

    Err(DevError::Runtime(
        "Could not find Podman socket".to_string(),
    ))
}

fn podman_exec_args(
    id: &str,
    cmd: &[String],
    user: Option<&str>,
    workdir: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-it".to_string()];
    if let Some(u) = user {
        args.push("--user".to_string());
        args.push(u.to_string());
    }
    if let Some(dir) = workdir {
        args.push("--workdir".to_string());
        args.push(dir.to_string());
    }
    args.push(id.to_string());
    args.extend(cmd.iter().cloned());
    args
}

impl ContainerRuntime for PodmanRuntime {
    fn runtime_name(&self) -> &'static str {
        "podman"
    }

    fn pull_image(&self, image: &str) -> BoxFut<'_, ()> {
        self.0.pull_image(image)
    }

    fn build_image(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        build_args: &std::collections::HashMap<String, String>,
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

    fn exec(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
        workdir: Option<&str>,
    ) -> BoxFut<'_, ExecResult> {
        self.0.exec(id, cmd, user, workdir)
    }

    fn exec_reports_missing_command(&self, error: &DevError) -> bool {
        self.0.exec_reports_missing_command(error)
    }

    fn exec_interactive(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
        workdir: Option<&str>,
    ) -> BoxFut<'_, i32> {
        // Podman's HTTP API doesn't reliably support interactive TTY exec via
        // bollard. Shell out to `podman exec -it` instead.
        let id = id.to_string();
        let cmd = cmd.to_vec();
        let user = user.map(|u| u.to_string());
        let workdir = workdir.map(|d| d.to_string());
        Box::pin(async move {
            let args = podman_exec_args(&id, &cmd, user.as_deref(), workdir.as_deref());

            let err = std::process::Command::new("podman").args(&args).exec();
            // exec() only returns on error
            Err(DevError::Runtime(format!(
                "Failed to exec into container: {err}"
            )))
        })
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
    use crate::runtime::{ContainerRuntime, WorkspaceMount};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Podman create delegates through the same bollard-backed create path as
    /// Docker. This uses a fake Unix-socket daemon rather than a live Podman
    /// daemon, so it proves the request body Podman sends through
    /// `ContainerRuntime::create_container`, not Podman's daemon behavior.
    #[tokio::test]
    async fn podman_create_container_sends_the_shared_bollard_create_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("podman.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 37\r\n\r\n{\"Id\":\"podman-created\",\"Warnings\":[]}",
                )
                .await
                .unwrap();
            request
        });

        let runtime = PodmanRuntime(
            BollardRuntime::connect_to_socket(&socket_path.to_string_lossy())
                .expect("building a podman client must not need a daemon"),
        );
        let mut config = ContainerConfig {
            image: "ubuntu:24.04".to_string(),
            name: "vsc-test".to_string(),
            labels: HashMap::new(),
            env: HashMap::from([("FROM_RUNARGS".to_string(), "1".to_string())]),
            mounts: vec![],
            volumes: vec![],
            ports: vec![],
            workspace_mount: Some(WorkspaceMount {
                source: std::path::PathBuf::from("/host/workspace"),
                target: "/workspace".to_string(),
            }),
            workspace_folder: Some("/workspace".to_string()),
            extra_args: vec![],
            entrypoint: None,
            init: true,
            privileged: true,
            cap_add: vec!["SYS_PTRACE".to_string()],
            security_opt: vec!["seccomp=unconfined".to_string()],
            userns_mode: Some("keep-id".to_string()),
        };
        config.labels.insert(
            "devcontainer.local_folder".to_string(),
            "/host/workspace".to_string(),
        );

        let id = (&runtime as &dyn ContainerRuntime)
            .create_container(&config)
            .await
            .expect("fake daemon should accept the create request");

        let request = server.await.unwrap();
        let body = request_json_body(&request);

        assert_eq!(id, "podman-created");
        assert!(
            request.starts_with("POST /containers/create"),
            "create must be sent through bollard's Docker-compatible create API, got: {request}"
        );
        assert!(
            request.contains("/containers/create?name=vsc-test"),
            "container name should be passed as create option, got: {request}"
        );
        assert_eq!(body["Image"], "ubuntu:24.04");
        assert_eq!(body["WorkingDir"], "/workspace");
        assert_eq!(body["Env"], serde_json::json!(["FROM_RUNARGS=1"]));
        assert_eq!(body["HostConfig"]["Init"], true);
        assert_eq!(body["HostConfig"]["Privileged"], true);
        assert_eq!(
            body["HostConfig"]["CapAdd"],
            serde_json::json!(["SYS_PTRACE"])
        );
        assert_eq!(
            body["HostConfig"]["SecurityOpt"],
            serde_json::json!(["seccomp=unconfined"])
        );
        assert_eq!(body["HostConfig"]["UsernsMode"], "keep-id");
    }

    async fn read_http_request(stream: &mut tokio::net::UnixStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "client closed before sending the request");
            buf.extend_from_slice(&chunk[..n]);
            if request_complete(&buf) {
                return String::from_utf8(buf).unwrap();
            }
        }
    }

    fn request_complete(buf: &[u8]) -> bool {
        let Some(header_end) = find_header_end(buf) else {
            return false;
        };
        let headers = std::str::from_utf8(&buf[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        buf.len() >= header_end + 4 + content_length
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn request_json_body(request: &str) -> serde_json::Value {
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn interactive_exec_args_include_the_requested_workspace_folder() {
        let args = podman_exec_args(
            "container-id",
            &["bash".to_string()],
            Some("vscode"),
            Some("/srv/app/packages/api"),
        );

        assert_eq!(
            args,
            vec![
                "exec",
                "-it",
                "--user",
                "vscode",
                "--workdir",
                "/srv/app/packages/api",
                "container-id",
                "bash",
            ]
        );
    }
}
