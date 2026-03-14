use crate::devcontainer::config::{DevcontainerConfig, LifecycleCommand};
use crate::devcontainer::features::ResolvedFeature;
use crate::error::DevError;
use crate::runtime::{ContainerRuntime, ExecResult};

/// Execute all lifecycle hooks in the devcontainer spec order.
///
/// Hooks run in order:
/// 1. onCreateCommand  (feature hooks first, then devcontainer.json)
/// 2. updateContentCommand
/// 3. postCreateCommand (feature hooks first, then devcontainer.json)
/// 4. postStartCommand  (feature hooks first, then devcontainer.json)
///
/// `postAttachCommand` is not run here as it requires an attached session.
/// Use [`run_post_attach_hooks`] for that.
pub async fn run_lifecycle_hooks<R: ContainerRuntime + ?Sized>(
    runtime: &R,
    container_id: &str,
    config: &DevcontainerConfig,
    user: Option<&str>,
    features: Option<&[ResolvedFeature]>,
) -> Result<(), DevError> {
    let empty = Vec::new();
    let features = features.unwrap_or(&empty);

    // onCreateCommand: features first (in dependency order), then config
    for f in features {
        if let Some(ref cmd) = f.lifecycle_hooks.on_create_command {
            run_hook(runtime, container_id, &format!("onCreateCommand [{}]", f.id), cmd, user).await?;
        }
    }
    if let Some(ref cmd) = config.on_create_command {
        run_hook(runtime, container_id, "onCreateCommand", cmd, user).await?;
    }

    // updateContentCommand: config only (features don't declare this)
    if let Some(ref cmd) = config.update_content_command {
        run_hook(runtime, container_id, "updateContentCommand", cmd, user).await?;
    }

    // postCreateCommand: features first, then config
    for f in features {
        if let Some(ref cmd) = f.lifecycle_hooks.post_create_command {
            run_hook(runtime, container_id, &format!("postCreateCommand [{}]", f.id), cmd, user).await?;
        }
    }
    if let Some(ref cmd) = config.post_create_command {
        run_hook(runtime, container_id, "postCreateCommand", cmd, user).await?;
    }

    // postStartCommand: features first, then config
    for f in features {
        if let Some(ref cmd) = f.lifecycle_hooks.post_start_command {
            run_hook(runtime, container_id, &format!("postStartCommand [{}]", f.id), cmd, user).await?;
        }
    }
    if let Some(ref cmd) = config.post_start_command {
        run_hook(runtime, container_id, "postStartCommand", cmd, user).await?;
    }

    Ok(())
}

/// Execute `postAttachCommand` hooks from features and the devcontainer config.
///
/// This should be called when attaching to an existing container via an IDE
/// integration (e.g., VS Code Remote Containers), not on every exec/shell invocation.
#[allow(dead_code)]
pub async fn run_post_attach_hooks<R: ContainerRuntime + ?Sized>(
    runtime: &R,
    container_id: &str,
    config: &DevcontainerConfig,
    user: Option<&str>,
    features: Option<&[ResolvedFeature]>,
) -> Result<(), DevError> {
    let empty = Vec::new();
    let features = features.unwrap_or(&empty);

    for f in features {
        if let Some(ref cmd) = f.lifecycle_hooks.post_attach_command {
            run_hook(runtime, container_id, &format!("postAttachCommand [{}]", f.id), cmd, user).await?;
        }
    }
    if let Some(ref cmd) = config.post_attach_command {
        run_hook(runtime, container_id, "postAttachCommand", cmd, user).await?;
    }

    Ok(())
}

async fn run_hook<R: ContainerRuntime + ?Sized>(
    runtime: &R,
    container_id: &str,
    name: &str,
    cmd: &LifecycleCommand,
    user: Option<&str>,
) -> Result<(), DevError> {
    match cmd {
        LifecycleCommand::Single(command) => {
            eprintln!("[lifecycle] Running {name}: {command}");
            let args = vec![
                "sh".to_string(),
                "-c".to_string(),
                command.clone(),
            ];
            let result = runtime.exec(container_id, &args, user).await?;
            check_result(name, command, &result)?;
        }
        LifecycleCommand::Multiple(commands) => {
            for command in commands {
                eprintln!("[lifecycle] Running {name}: {command}");
                let args = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    command.clone(),
                ];
                let result = runtime.exec(container_id, &args, user).await?;
                check_result(name, command, &result)?;
            }
        }
        LifecycleCommand::Parallel(commands) => {
            run_parallel(runtime, container_id, name, commands, user).await?;
        }
    }
    Ok(())
}

/// Run named commands in parallel using tokio tasks (Gap 14).
async fn run_parallel<R: ContainerRuntime + ?Sized>(
    runtime: &R,
    container_id: &str,
    name: &str,
    commands: &std::collections::HashMap<String, String>,
    user: Option<&str>,
) -> Result<(), DevError> {
    use futures_util::future::join_all;

    let futures: Vec<_> = commands
        .iter()
        .map(|(label, command)| {
            let label = label.clone();
            let command = command.clone();
            let container_id = container_id.to_string();
            let name = name.to_string();
            let user = user.map(|u| u.to_string());

            async move {
                eprintln!("[lifecycle] Running {name} ({label}): {command}");
                let args = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    command.clone(),
                ];
                let result = runtime.exec(&container_id, &args, user.as_deref()).await?;
                check_result(&name, &command, &result)?;
                Ok::<(), DevError>(())
            }
        })
        .collect();

    let results = join_all(futures).await;
    for result in results {
        result?;
    }

    Ok(())
}

fn check_result(hook_name: &str, command: &str, result: &ExecResult) -> Result<(), DevError> {
    if result.exit_code != 0 {
        eprintln!(
            "[lifecycle] {hook_name} failed (exit {}):\nstdout: {}\nstderr: {}",
            result.exit_code, result.stdout, result.stderr
        );
        return Err(DevError::LifecycleHook {
            command: command.to_string(),
            code: result.exit_code,
        });
    }
    Ok(())
}
