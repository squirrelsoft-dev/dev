use crate::devcontainer::config::{DevcontainerConfig, LifecycleCommand};
use crate::error::DevError;
use crate::runtime::{ContainerRuntime, ExecResult};

/// Execute all lifecycle hooks in the devcontainer spec order.
///
/// Hooks run in order:
/// 1. onCreateCommand
/// 2. updateContentCommand
/// 3. postCreateCommand
/// 4. postStartCommand
///
/// `postAttachCommand` is not run here as it requires an attached session.
pub async fn run_lifecycle_hooks<R: ContainerRuntime + ?Sized>(
    runtime: &R,
    container_id: &str,
    config: &DevcontainerConfig,
    user: Option<&str>,
) -> Result<(), DevError> {
    let hooks: &[(&str, &Option<LifecycleCommand>)] = &[
        ("onCreateCommand", &config.on_create_command),
        ("updateContentCommand", &config.update_content_command),
        ("postCreateCommand", &config.post_create_command),
        ("postStartCommand", &config.post_start_command),
    ];

    for (name, hook) in hooks {
        if let Some(cmd) = hook {
            run_hook(runtime, container_id, name, cmd, user).await?;
        }
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
            // Run named commands sequentially for simplicity.
            // A full implementation would run these in parallel.
            for (label, command) in commands {
                eprintln!("[lifecycle] Running {name} ({label}): {command}");
                let args = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    command.clone(),
                ];
                let result = runtime.exec(container_id, &args, user).await?;
                check_result(name, command, &result)?;
            }
        }
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
