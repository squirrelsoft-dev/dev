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

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{self, Poll};

use devcontainer::commands::down::run_with_runtime;
use devcontainer::runtime::{
    BoxFut, ContainerConfig, ContainerInfo, ContainerState, ExecResult, ImageMetadata,
};

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
    Box::pin(async { v.map_err(|e| devcontainer::error::DevError::Runtime(e.0)) })
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

impl devcontainer::runtime::ContainerRuntime for FakeRuntime {
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

    fn exec(&self, _id: &str, _cmd: &[String], _user: Option<&str>) -> BoxFut<'static, ExecResult> {
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

    fn list_containers(&self, _label_filters: &[String]) -> BoxFut<'static, Vec<ContainerInfo>> {
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
    ) -> BoxFut<'static, devcontainer::runtime::AttachedExec> {
        as_fut(Ok(devcontainer::runtime::AttachedExec {
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

    let res = run_with_runtime(workspace, &rt, true).await;

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

    let res = run_with_runtime(workspace, &rt, true).await;

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

    let res = run_with_runtime(workspace, &rt, true).await;

    // remove_container should have been called for both containers.
    // The Gemini review: removal success after stop failure does not fail
    // the command, so both are removed despite stop failures.
    assert_eq!(rt.removed.load(Ordering::SeqCst), 2);
    // The overall call succeeds (all removals happened despite stop failures).
    assert!(
        res.is_ok(),
        "run_with_runtime should succeed (all removals went through despite stop failures: {:?})",
        res.err()
    );
}
