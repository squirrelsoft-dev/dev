pub mod build;
pub mod error;
pub mod models;
pub mod routes;
pub mod xpc;

use std::os::fd::RawFd;
use std::path::Path;

use error::AppleContainerError;
use models::{ContainerConfiguration, ContainerSnapshot, ContainerStats, ProcessConfiguration};
use routes::{XpcKey, XpcRoute, SERVICE_NAME};
use xpc::connection::XpcConnection;
use xpc::message::XpcMessage;

/// Client for the Apple Container XPC API (`com.apple.container.apiserver`).
pub struct AppleContainerClient {
    connection: XpcConnection,
}

impl AppleContainerClient {
    /// Connect to the Apple Container daemon via XPC.
    pub fn connect() -> Result<Self, AppleContainerError> {
        let connection = XpcConnection::connect(SERVICE_NAME)?;
        Ok(Self { connection })
    }

    /// Ping the daemon to verify connectivity.
    pub async fn ping(&self) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::Ping.as_str());
        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// List all containers, optionally filtered.
    pub async fn list(&self) -> Result<Vec<ContainerSnapshot>, AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerList.as_str());
        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;

        let data = reply.get_data(XpcKey::CONTAINERS).unwrap_or_default();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let snapshots: Vec<ContainerSnapshot> = serde_json::from_slice(&data)?;
        Ok(snapshots)
    }

    /// Get a single container by ID.
    ///
    /// Uses `containerList` and filters, since `containerGet` does not return
    /// snapshot data under a discoverable key in the XPC reply.
    pub async fn get(&self, id: &str) -> Result<ContainerSnapshot, AppleContainerError> {
        let containers = self.list().await?;
        containers
            .into_iter()
            .find(|s| s.configuration.id == id)
            .ok_or_else(|| AppleContainerError::NotFound(id.to_string()))
    }

    /// Fetch the default kernel from the daemon for the given platform.
    ///
    /// Returns the raw JSON bytes of the `Kernel` struct, suitable for
    /// passing directly to [`create`](Self::create).
    pub async fn get_default_kernel(&self) -> Result<Vec<u8>, AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::GetDefaultKernel.as_str());

        let platform_json = serde_json::to_vec(&serde_json::json!({
            "os": "linux",
            "architecture": "arm64"
        }))?;
        msg.set_data(XpcKey::SYSTEM_PLATFORM, &platform_json);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;

        reply.get_data(XpcKey::KERNEL).ok_or_else(|| {
            AppleContainerError::XpcError("getDefaultKernel reply missing kernel data".to_string())
        })
    }

    /// Create a new container with the given configuration and kernel.
    ///
    /// `kernel` should be the raw JSON bytes from [`get_default_kernel`](Self::get_default_kernel).
    pub async fn create(
        &self,
        config: &ContainerConfiguration,
        kernel: &[u8],
    ) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerCreate.as_str());

        let config_json = serde_json::to_vec(config)?;
        msg.set_data(XpcKey::CONTAINER_CONFIG, &config_json);
        msg.set_data(XpcKey::KERNEL, kernel);
        let options_bytes = serde_json::to_vec(&serde_json::json!({"autoRemove": false}))?;
        msg.set_data(XpcKey::CONTAINER_OPTIONS, &options_bytes);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Bootstrap (start) a container, attaching stdio file descriptors.
    pub async fn bootstrap(
        &self,
        id: &str,
        stdin: RawFd,
        stdout: RawFd,
        stderr: RawFd,
    ) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerBootstrap.as_str());
        msg.set_string(XpcKey::ID, id);
        msg.set_fd(XpcKey::STDIN, stdin);
        msg.set_fd(XpcKey::STDOUT, stdout);
        msg.set_fd(XpcKey::STDERR, stderr);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Create and start a new process inside a running container.
    pub async fn create_process(
        &self,
        container_id: &str,
        process_id: &str,
        config: &ProcessConfiguration,
        stdin: RawFd,
        stdout: RawFd,
        stderr: RawFd,
    ) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerCreateProcess.as_str());
        msg.set_string(XpcKey::ID, container_id);
        msg.set_string(XpcKey::PROCESS_IDENTIFIER, process_id);

        let config_json = serde_json::to_vec(config)?;
        msg.set_data(XpcKey::PROCESS_CONFIG, &config_json);
        msg.set_fd(XpcKey::STDIN, stdin);
        msg.set_fd(XpcKey::STDOUT, stdout);
        msg.set_fd(XpcKey::STDERR, stderr);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Wait for a process to exit and return its exit code.
    /// The exit code is returned in the create_process reply when the process exits.
    /// For long-running processes, this polls the container state.
    pub fn get_exit_code(reply: &XpcMessage) -> i32 {
        reply.get_int64(XpcKey::EXIT_CODE) as i32
    }

    /// Send a signal to a container.
    pub async fn kill(&self, id: &str, signal: i32) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerKill.as_str());
        msg.set_string(XpcKey::ID, id);
        msg.set_int64(XpcKey::SIGNAL, signal as i64);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Gracefully stop a container.
    pub async fn stop(&self, id: &str) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerStop.as_str());
        msg.set_string(XpcKey::ID, id);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Delete a container.
    pub async fn delete(&self, id: &str, force: bool) -> Result<(), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerDelete.as_str());
        msg.set_string(XpcKey::ID, id);
        msg.set_bool(XpcKey::FORCE, force);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;
        Ok(())
    }

    /// Get log file descriptors for a container (stdout fd, stderr fd).
    pub async fn logs(&self, id: &str) -> Result<(RawFd, RawFd), AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerLogs.as_str());
        msg.set_string(XpcKey::ID, id);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;

        let stdout_fd = reply.dup_fd(XpcKey::STDOUT).ok_or_else(|| {
            AppleContainerError::XpcError("logs reply missing stdout fd".to_string())
        })?;
        let stderr_fd = reply.dup_fd(XpcKey::STDERR).ok_or_else(|| {
            // Clean up stdout fd on error.
            unsafe { libc::close(stdout_fd) };
            AppleContainerError::XpcError("logs reply missing stderr fd".to_string())
        })?;

        Ok((stdout_fd, stderr_fd))
    }

    /// Get container statistics.
    pub async fn stats(&self, id: &str) -> Result<ContainerStats, AppleContainerError> {
        let msg = XpcMessage::with_route(XpcRoute::ContainerStats.as_str());
        msg.set_string(XpcKey::ID, id);

        let reply = self.connection.send_async(&msg).await?;
        reply.check_error()?;

        let data = reply.get_data(XpcKey::STATISTICS).ok_or_else(|| {
            AppleContainerError::XpcError("stats reply missing data".to_string())
        })?;
        let stats: ContainerStats = serde_json::from_slice(&data)?;
        Ok(stats)
    }

    /// Dial a container on a port, returning a vsock file descriptor.
    pub async fn dial(&self, id: &str, port: u32) -> Result<RawFd, AppleContainerError> {
        build::dial_container(&self.connection, id, port).await
    }

    /// Build an image using the Apple Containers BuildKit builder.
    pub async fn build(
        &self,
        dockerfile: &str,
        context: &Path,
        tag: &str,
        no_cache: bool,
        verbose: bool,
    ) -> Result<(), AppleContainerError> {
        build::build_image(&self.connection, dockerfile, context, tag, no_cache, verbose).await
    }
}
