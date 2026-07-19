use std::path::Path;

use crate::devcontainer::DevcontainerConfig;
use crate::runtime::{ContainerState, detect_runtime};
use crate::util::{container_name, find_devcontainer_config, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    remove: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;

    // Try compose-aware teardown first.
    if let Ok(config_path) = find_devcontainer_config(workspace) {
        if let Ok(config) = DevcontainerConfig::from_path(&config_path) {
            if config.is_compose() {
                return run_compose_down(
                    workspace,
                    &config,
                    &config_path,
                    runtime.runtime_name(),
                    remove,
                )
                .await;
            }
        }
    }

    // Non-compose: label-based container stop/remove.
    run_with_runtime(workspace, &*runtime, remove, crate::caddy::unregister_site).await
}

/// Internal: run the non-compose teardown path against a specific runtime.
/// Caddy cleanup is injected so tests can stub it; production callers pass
/// `crate::caddy::unregister_site`.
pub async fn run_with_runtime(
    workspace: &Path,
    runtime: &dyn crate::runtime::ContainerRuntime,
    remove: bool,
    unregister_caddy: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    if containers.is_empty() {
        println!("No containers found for this workspace.");
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();

    for container in &containers {
        let stopped = if container.state == ContainerState::Running {
            eprintln!("Stopping container '{}'...", container.name);
            stop_container_safe(runtime, &container.id).await
        } else {
            Ok(())
        };

        if remove {
            eprintln!("Removing container '{}'...", container.name);
            if let Err(e) = runtime.remove_container(&container.id).await {
                if let Err(stop_err) = &stopped {
                    failures.push(format!(
                        "stop_container {} failed: {}",
                        container.name, stop_err,
                    ));
                }
                failures.push(format!("remove_container {} failed: {}", container.name, e,));
            } else {
                println!("Container '{}' removed.", container.name);
            }
        } else {
            match stopped {
                Ok(()) => println!("Container '{}' stopped.", container.name),
                Err(e) => {
                    failures.push(format!("stop_container {} failed: {}", container.name, e,));
                }
            }
        }
    }

    if !failures.is_empty() {
        Err(anyhow::anyhow!("{}", failures.join("; ")))
    } else if let Err(e) = unregister_caddy(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
        Ok(())
    } else {
        Ok(())
    }
}

/// Stop a container, but trust observed state over the call's return value.
///
/// If `stop_container` returns `Ok`, the call succeeded (no re-check needed).
/// If it returns `Err`, we re-check via `inspect_container`: if the container
/// is no longer `Running`, we treat it as actually stopped and swallow the
/// error. Only treat a failure as genuine if the container is still running
/// after the re-check.
async fn stop_container_safe(
    runtime: &dyn crate::runtime::ContainerRuntime,
    id: &str,
) -> anyhow::Result<()> {
    if runtime.stop_container(id).await.is_ok() {
        return Ok(());
    }

    // Re-check state: if it stopped while reporting failure, honour that.
    match runtime.inspect_container(id).await {
        Ok(info) => {
            if info.state != ContainerState::Running {
                return Ok(());
            }
        }
        Err(e) => {
            eprintln!("Warning: inspect_container failed after stop_container: {e}");
        }
    }
    anyhow::bail!(
        "stop_container failed and container {} still appears running",
        id
    );
}

/// Tear down a Docker Compose-based workspace.
async fn run_compose_down(
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
    runtime_name: &str,
    remove: bool,
) -> anyhow::Result<()> {
    let compose_data = config.docker_compose_file.as_ref().unwrap();
    let compose_files = compose_data.files();
    let devcontainer_dir = config_path.parent().unwrap();
    let project_name = container_name(workspace);

    if remove {
        eprintln!("Removing compose services...");
        crate::runtime::compose::compose_down(
            runtime_name,
            &compose_files,
            devcontainer_dir,
            &project_name,
        )
        .await?;
        println!("Compose services removed.");
    } else {
        eprintln!("Stopping compose services...");
        crate::runtime::compose::compose_stop(
            runtime_name,
            &compose_files,
            devcontainer_dir,
            &project_name,
        )
        .await?;
        println!("Compose services stopped.");
    }

    if let Err(e) = crate::caddy::unregister_site(workspace) {
        eprintln!("Warning: Caddy cleanup failed: {e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Regression test for issue #17: a failed `stop_container` should not skip
    //! removal, and a failure on one container should not abandon the rest.
    //!
    //! Uses a minimal `ContainerRuntime` mock whose `stop_container` returns Err
    //! for the first container but whose `inspect_container` reports it as Stopped,
    //! and whose `remove_container` always returns Ok.
    //!
    //! Verifies:
    //!   1. `remove_container` is still called for the container whose stop failed.
    //!   2. A stop failure on container 0 does not skip removal for container 1.
    //!   3. Removal success after stop failure does not fail the command.
    use super::run_with_runtime;
    use crate::error::DevError;
    use crate::runtime::{
        AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
        ExecResult, ImageMetadata,
    };
    use std::collections::HashMap;
    use std::io;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::task::{self, Poll};

    // ---------------------------------------------------------------------------
    // Minimal fake runtime
    // ---------------------------------------------------------------------------

    struct FakeRuntime {
        /// Per-container ID stop response.
        stop_responses: HashMap<String, Result<(), StubDevError>>,
        /// Per-container ID state reported by inspect.
        inspect_states: HashMap<String, ContainerState>,
        /// Per-container ID remove response.
        remove_responses: HashMap<String, Result<(), StubDevError>>,
        /// Track how many containers remove_container was called for (count of Ok).
        removed: Arc<AtomicU32>,
    }

    #[derive(Debug)]
    struct StubDevError(String);
    impl std::fmt::Display for StubDevError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for StubDevError {}
    impl Clone for StubDevError {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                stop_responses: HashMap::new(),
                inspect_states: HashMap::new(),
                remove_responses: HashMap::new(),
                removed: Arc::new(AtomicU32::new(0)),
            }
        }

        fn fail(&self, msg: &str) -> Result<(), StubDevError> {
            Err(StubDevError(msg.to_string()))
        }
    }

    /// Turn an owned Result into a Boxed future.
    fn as_fut<T: Send + 'static>(v: Result<T, StubDevError>) -> BoxFut<'static, T> {
        Box::pin(async { v.map_err(|e| DevError::Runtime(e.0)) })
    }

    /// Minimal AsyncRead stub: immediately EOFs.
    struct NoopRead;
    impl tokio::io::AsyncRead for NoopRead {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Minimal AsyncWrite stub: writes all, immediately idle.
    struct NoopWrite;
    impl tokio::io::AsyncWrite for NoopWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl ContainerRuntime for FakeRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake"
        }

        fn pull_image(&self, _image: &str) -> BoxFut<'static, ()> {
            as_fut(Ok(()))
        }

        fn build_image(
            &self,
            _dockerfile: &str,
            _context: &std::path::Path,
            _tag: &str,
            _build_args: &HashMap<String, String>,
            _no_cache: bool,
            _verbose: bool,
        ) -> BoxFut<'static, ()> {
            as_fut(Ok(()))
        }

        fn create_container(&self, _config: &ContainerConfig) -> BoxFut<'static, String> {
            as_fut(Ok("fake-id".to_string()))
        }

        fn start_container(&self, _id: &str) -> BoxFut<'static, ()> {
            as_fut(Ok(()))
        }

        fn stop_container(&self, id: &str) -> BoxFut<'static, ()> {
            let v = self.stop_responses.get(id).cloned().unwrap_or(Ok(()));
            as_fut(v)
        }

        fn remove_container(&self, id: &str) -> BoxFut<'static, ()> {
            let res = self.remove_responses.get(id).cloned().unwrap_or(Ok(()));
            if res.is_ok() {
                self.removed.fetch_add(1, Ordering::SeqCst);
            }
            as_fut(res)
        }

        fn exec(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'static, ExecResult> {
            as_fut(Ok(ExecResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }))
        }

        fn exec_interactive(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'static, ()> {
            as_fut(Ok(()))
        }

        fn inspect_container(&self, id: &str) -> BoxFut<'static, ContainerInfo> {
            let state = self
                .inspect_states
                .get(id)
                .cloned()
                .unwrap_or(ContainerState::Running);
            as_fut(Ok(ContainerInfo {
                id: id.to_string(),
                name: format!("fake-{}", id),
                state,
                labels: HashMap::new(),
                image: "fake/image:latest".to_string(),
            }))
        }

        fn list_containers(
            &self,
            _label_filters: &[String],
        ) -> BoxFut<'static, Vec<ContainerInfo>> {
            let info = vec![
                ContainerInfo {
                    id: "1111".to_string(),
                    name: "container-0".to_string(),
                    state: ContainerState::Running,
                    labels: HashMap::new(),
                    image: "fake/image:latest".to_string(),
                },
                ContainerInfo {
                    id: "2222".to_string(),
                    name: "container-1".to_string(),
                    state: ContainerState::Running,
                    labels: HashMap::new(),
                    image: "fake/image:latest".to_string(),
                },
            ];
            as_fut(Ok(info))
        }

        fn image_exists(&self, _image: &str) -> BoxFut<'static, bool> {
            as_fut(Ok(true))
        }

        fn inspect_image_metadata(&self, _image: &str) -> BoxFut<'static, ImageMetadata> {
            as_fut(Ok(ImageMetadata::default()))
        }

        fn exec_attached(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'static, AttachedExec> {
            as_fut(Ok(AttachedExec {
                stdin: Box::pin(NoopWrite),
                stdout: Box::pin(NoopRead),
            }))
        }
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn failed_stop_still_triggers_remove() {
        let mut rt = FakeRuntime::new();
        let workspace = Path::new("/tmp/fake-workspace");

        // Container 0: stop fails, but inspect reports Stopped.
        rt.stop_responses
            .insert("1111".to_string(), rt.fail("XPC timeout"));
        rt.inspect_states
            .insert("1111".to_string(), ContainerState::Stopped);
        // Container 1: stop succeeds (default Ok), inspect reports Stopped too.
        rt.inspect_states
            .insert("2222".to_string(), ContainerState::Stopped);

        let res = run_with_runtime(workspace, &rt, true, |_: &Path| Ok(())).await;

        // remove_container must be called for each container, including the one whose stop failed.
        assert_eq!(rt.removed.load(Ordering::SeqCst), 2);
        // The overall call succeeds: no stranded container, all removals went through.
        assert!(
            res.is_ok(),
            "run_with_runtime should succeed, got: {:?}",
            res.err()
        );
    }

    #[tokio::test]
    async fn removal_success_after_stop_failure_does_not_fail_the_command() {
        let mut rt = FakeRuntime::new();
        let workspace = Path::new("/tmp/fake-workspace");

        // Container 0: stop fails, inspect reports Stopped, remove succeeds.
        // Container 1: same — stop fails, inspect Stopped, remove succeeds.
        rt.stop_responses
            .insert("1111".to_string(), rt.fail("XPC timeout"));
        rt.inspect_states
            .insert("1111".to_string(), ContainerState::Stopped);
        rt.remove_responses.insert("1111".to_string(), Ok(()));
        rt.stop_responses
            .insert("2222".to_string(), rt.fail("XPC timeout"));
        rt.inspect_states
            .insert("2222".to_string(), ContainerState::Stopped);
        rt.remove_responses.insert("2222".to_string(), Ok(()));

        let res = run_with_runtime(workspace, &rt, true, |_: &Path| Ok(())).await;

        // remove_container was called for each container — no error.
        assert_eq!(rt.removed.load(Ordering::SeqCst), 2);
        // The overall call succeeds even though stop failed.
        assert!(
            res.is_ok(),
            "run_with_runtime should succeed (removal succeeded despite stop failure: {:?})",
            res.err()
        );
    }

    #[tokio::test]
    async fn stop_failure_on_one_container_does_not_abandon_the_rest() {
        let mut rt = FakeRuntime::new();
        let workspace = Path::new("/tmp/fake-workspace");

        // Container 0: stop fails, inspect reports Stopped.
        rt.stop_responses
            .insert("1111".to_string(), rt.fail("XPC timeout"));
        rt.inspect_states
            .insert("1111".to_string(), ContainerState::Stopped);
        // Container 1: stop fails AND it's still Running -> genuine failure.
        rt.stop_responses
            .insert("2222".to_string(), rt.fail("XPC timeout"));
        rt.inspect_states
            .insert("2222".to_string(), ContainerState::Running);
        // Both: remove succeeds (no extra failures from remove).
        rt.remove_responses.insert("1111".to_string(), Ok(()));
        rt.remove_responses.insert("2222".to_string(), Ok(()));

        let res = run_with_runtime(workspace, &rt, true, |_: &Path| Ok(())).await;

        // remove_container should have been called for both containers: a stop
        // failure must not skip removal, so both are removed.
        assert_eq!(rt.removed.load(Ordering::SeqCst), 2);
        // The overall call succeeds (all removals happened despite stop failures).
        assert!(
            res.is_ok(),
            "run_with_runtime should succeed (all removals went through despite stop failures: {:?})",
            res.err()
        );
    }
}
