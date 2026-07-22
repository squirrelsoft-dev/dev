use std::path::Path;

use crate::devcontainer::compose::load_workspace_config_or_warn;
use crate::runtime::{ContainerState, detect_runtime, resolve_remote_user};
use crate::util::{workspace_folder_name, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    user: Option<&str>,
    cmd: &[String],
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    run_with_runtime(workspace, runtime.as_ref(), user, cmd).await
}

pub(crate) async fn run_with_runtime(
    workspace: &Path,
    runtime: &dyn crate::runtime::ContainerRuntime,
    user: Option<&str>,
    cmd: &[String],
) -> anyhow::Result<()> {
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| {
            anyhow::anyhow!("No running container found for this workspace. Run `dev up` first.")
        })?;

    // Use explicit --user flag, falling back to remoteUser from config or image metadata
    let config =
        load_workspace_config_or_warn(workspace, runtime.runtime_name()).map(|(_, config)| config);

    let resolved_user = if user.is_some() {
        user.map(|u| u.to_string())
    } else {
        let config_user = config
            .as_ref()
            .and_then(|config| config.remote_user.clone());
        resolve_remote_user(runtime, &container.image, config_user.as_deref()).await?
    };
    let effective_user = resolved_user.as_deref();

    let workdir = match config.as_ref() {
        Some(config) => config.workspace_folder_path(workspace, effective_user)?,
        None => format!("/workspaces/{}", workspace_folder_name(workspace)),
    };

    let result = runtime
        .exec(&container.id, cmd, effective_user, Some(&workdir))
        .await?;

    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    if result.exit_code != 0 {
        std::process::exit(result.exit_code);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run_with_runtime;
    use crate::error::DevError;
    use crate::runtime::{
        AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
        ExecResult, ImageMetadata,
    };
    use crate::util::workspace_labels;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn unused<T>() -> BoxFut<'static, T> {
        Box::pin(async { Err(DevError::Runtime("unused fake runtime method".into())) })
    }

    type ExecCall = (Vec<String>, Option<String>, Option<String>);

    struct ExecFakeRuntime {
        containers: Vec<ContainerInfo>,
        execs: Arc<Mutex<Vec<ExecCall>>>,
    }

    impl ExecFakeRuntime {
        fn running_for(workspace: &Path, config_path: &Path) -> Self {
            Self {
                containers: vec![ContainerInfo {
                    id: "container-id".to_string(),
                    name: "container".to_string(),
                    state: ContainerState::Running,
                    labels: workspace_labels(workspace, Some(config_path))
                        .into_iter()
                        .collect(),
                    image: "ubuntu:24.04".to_string(),
                }],
                execs: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn execs(&self) -> Vec<ExecCall> {
            self.execs.lock().unwrap().clone()
        }
    }

    impl ContainerRuntime for ExecFakeRuntime {
        fn runtime_name(&self) -> &'static str {
            "docker"
        }

        fn pull_image(&self, _image: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn build_image(
            &self,
            _dockerfile: &str,
            _context: &Path,
            _tag: &str,
            _build_args: &HashMap<String, String>,
            _no_cache: bool,
            _verbose: bool,
        ) -> BoxFut<'_, ()> {
            unused()
        }

        fn create_container(&self, _config: &ContainerConfig) -> BoxFut<'_, String> {
            unused()
        }

        fn start_container(&self, _id: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn stop_container(&self, _id: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn remove_container(&self, _id: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn exec(
            &self,
            _id: &str,
            cmd: &[String],
            user: Option<&str>,
            workdir: Option<&str>,
        ) -> BoxFut<'_, ExecResult> {
            self.execs.lock().unwrap().push((
                cmd.to_vec(),
                user.map(str::to_string),
                workdir.map(str::to_string),
            ));
            Box::pin(async {
                Ok(ExecResult {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            })
        }

        fn exec_interactive(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
            _workdir: Option<&str>,
        ) -> BoxFut<'_, i32> {
            unused()
        }

        fn inspect_container(&self, _id: &str) -> BoxFut<'_, ContainerInfo> {
            unused()
        }

        fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
            let filters: Vec<(String, String)> = label_filters
                .iter()
                .map(|filter| {
                    let (key, value) = filter.split_once('=').unwrap_or((filter, ""));
                    (key.to_string(), value.to_string())
                })
                .collect();
            let containers = self.containers.clone();
            Box::pin(async move {
                Ok(containers
                    .into_iter()
                    .filter(|container| {
                        filters.iter().all(|(key, value)| {
                            container.labels.get(key).is_some_and(|got| got == value)
                        })
                    })
                    .collect())
            })
        }

        fn image_exists(&self, _image: &str) -> BoxFut<'_, bool> {
            unused()
        }

        fn inspect_image_metadata(&self, _image: &str) -> BoxFut<'_, ImageMetadata> {
            Box::pin(async { Ok(ImageMetadata::default()) })
        }

        fn exec_attached(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'_, AttachedExec> {
            unused()
        }
    }

    #[tokio::test]
    async fn one_off_exec_runs_in_the_resolved_workspace_folder() {
        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        std::fs::create_dir_all(&devcontainer_dir).unwrap();
        let config_path = devcontainer_dir.join("devcontainer.json");
        std::fs::write(
            &config_path,
            r#"{
                "image": "ubuntu:24.04",
                "workspaceMount": "source=${localWorkspaceFolder},target=/srv/app,type=bind",
                "workspaceFolder": "/srv/app/packages/api"
            }"#,
        )
        .unwrap();
        let runtime = ExecFakeRuntime::running_for(workspace.path(), &config_path);

        run_with_runtime(
            workspace.path(),
            &runtime,
            None,
            &["cargo".to_string(), "test".to_string()],
        )
        .await
        .expect("dev exec should run the command");

        let execs = runtime.execs();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].0, vec!["cargo".to_string(), "test".to_string()]);
        assert_eq!(execs[0].2.as_deref(), Some("/srv/app/packages/api"));
    }
}
