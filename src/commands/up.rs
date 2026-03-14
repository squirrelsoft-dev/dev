use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::devcontainer::{
    DevcontainerConfig, Recipe, compose_and_write, download_features,
    merge_feature_capabilities, resolve_features,
    run_lifecycle_hooks, stage_feature_context, substitute_variables,
    substitute_variables_with_user,
};
use crate::devcontainer::features::{generate_feature_dockerfile_with_opts, order_features};
use crate::devcontainer::lockfile::{handle_lockfile, lockfile_path};
use crate::runtime::{
    BindMount, ContainerConfig, ContainerRuntime, ContainerState, PortMapping, WorkspaceMount,
    detect_runtime, resolve_remote_user,
};
use crate::util::{
    container_name, find_config_source, workspace_folder_name, workspace_label, ConfigSource,
};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    rebuild: bool,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    buildkit: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let config_path = match find_config_source(workspace)? {
        ConfigSource::Direct(path) => path,
        ConfigSource::Recipe(recipe_path) => {
            let recipe = Recipe::from_path(&recipe_path)?;
            compose_and_write(&recipe, runtime.runtime_name())?
        }
    };
    let config = DevcontainerConfig::from_path(&config_path)?;

    // Run initializeCommand on the host before anything else (Gap 9).
    if let Some(ref init_cmd) = config.initialize_command {
        run_initialize_command(init_cmd, workspace).await?;
    }

    let (label_key, label_val) = workspace_label(workspace);
    let filter = format!("{label_key}={label_val}");
    let existing = runtime.list_containers(&filter).await?;

    // Handle existing container
    if let Some(container) = existing.first() {
        match container.state {
            ContainerState::Running if !rebuild => {
                println!("Container '{}' is already running.", container.name);
                return Ok(());
            }
            ContainerState::Stopped if !rebuild => {
                println!("Starting existing container '{}'...", container.name);
                runtime.start_container(&container.id).await?;
                if config.post_start_command.is_some() {
                    let user = resolve_remote_user(
                        runtime.as_ref(),
                        &container.image,
                        config.remote_user.as_deref(),
                    ).await?;
                    run_lifecycle_hooks(runtime.as_ref(), &container.id, &config, user.as_deref(), None).await?;
                }
                println!("Container '{}' started.", container.name);
                return Ok(());
            }
            _ => {
                // Rebuild: remove existing
                eprintln!("Removing existing container '{}'...", container.name);
                if container.state == ContainerState::Running {
                    runtime.stop_container(&container.id).await?;
                }
                runtime.remove_container(&container.id).await?;
            }
        }
    }

    // Use the same image tag that `dev build` produces so we can reuse it.
    let initial_features = resolve_features(&config)?;
    let has_features = !initial_features.is_empty();
    let needs_build = config.build.is_some() || has_features;
    let final_tag = format!("{}-features", container_name(workspace));

    // Resolve the .devcontainer directory for local feature paths and lockfile.
    let devcontainer_dir: Option<PathBuf> = config_path.parent().map(|p| p.to_path_buf());

    // Track ordered features for later use (capabilities, lifecycle hooks).
    let mut ordered_features = Vec::new();

    let final_image = if !needs_build {
        // Image-based config with no features — use the remote image directly.
        let image = config.image.as_ref()
            .ok_or_else(|| anyhow::anyhow!("devcontainer.json must specify either 'image' or 'build.dockerfile'"))?;
        eprintln!("Pulling image '{image}'...");
        runtime.pull_image(image).await?;
        image.clone()
    } else if !rebuild && !no_cache && runtime.image_exists(&final_tag).await? {
        // Image already built (e.g. by `dev build`), skip rebuild.
        eprintln!("Image '{final_tag}' already exists, skipping build.");
        final_tag
    } else {
        // Determine base image
        let base_image = if let Some(ref image) = config.image {
            eprintln!("Pulling image '{image}'...");
            runtime.pull_image(image).await?;
            image.clone()
        } else if let Some(ref build) = config.build {
            let context_dir = config_path
                .parent()
                .unwrap()
                .join(build.context.as_deref().unwrap_or("."));
            let dockerfile_path = config_path
                .parent()
                .unwrap()
                .join(&build.dockerfile);
            let dockerfile_content = std::fs::read_to_string(&dockerfile_path)?;
            if !has_features {
                // No features — build directly with the final tag.
                eprintln!("Building image from Dockerfile...");
                runtime
                    .build_image(&dockerfile_content, &context_dir, &final_tag, no_cache, verbose)
                    .await?;
                final_tag.clone()
            } else {
                let base_tag = format!("{final_tag}-base");
                eprintln!("Building image from Dockerfile...");
                runtime
                    .build_image(&dockerfile_content, &context_dir, &base_tag, no_cache, verbose)
                    .await?;
                base_tag
            }
        } else {
            anyhow::bail!("devcontainer.json must specify either 'image' or 'build.dockerfile'");
        };

        // Handle features
        if has_features {
            let mut features = initial_features;
            let original_count = features.len();
            eprintln!("Downloading {} feature(s)...", original_count);
            if verbose {
                for f in &features {
                    eprintln!("  Feature: {} ({}:{})", f.id, f.oci_ref, f.version);
                }
            }
            download_features(&mut features, devcontainer_dir.as_deref()).await?;

            if features.len() > original_count {
                eprintln!(
                    "Resolved {} transitive dependencies",
                    features.len() - original_count
                );
            }

            // Lockfile handling (Gap 11).
            if let Some(ref dc_dir) = devcontainer_dir {
                let lf_path = lockfile_path(dc_dir);
                handle_lockfile(&lf_path, &features, frozen_lockfile)?;
            }

            let ordered = order_features(&features);
            if verbose {
                eprintln!("Feature install order:");
                for (i, f) in ordered.iter().enumerate() {
                    eprintln!("  {}: {}{}", i + 1, f.id, if f.is_dependency { " (dependency)" } else { "" });
                }
            }
            let staging_dir = stage_feature_context(&ordered)?;
            let feature_user = resolve_remote_user(
                runtime.as_ref(),
                &base_image,
                config.remote_user.as_deref(),
            ).await?;
            let dockerfile = generate_feature_dockerfile_with_opts(
                &base_image,
                &ordered,
                feature_user.as_deref(),
                &config,
                buildkit,
            );
            if verbose {
                eprintln!("Features Dockerfile:\n{dockerfile}");
            }
            eprintln!("Building features image...");
            let result = runtime
                .build_image(&dockerfile, &staging_dir, &final_tag, no_cache, verbose)
                .await;
            let _ = std::fs::remove_dir_all(&staging_dir);
            result?;

            ordered_features = ordered;
        }

        final_tag
    };

    // Build container config
    let name = container_name(workspace);
    let folder_name = workspace_folder_name(workspace);

    let mut labels = HashMap::new();
    labels.insert(label_key, label_val);

    // Substitute devcontainer variables in env values
    let mut env = HashMap::new();
    if let Some(ref container_env) = config.container_env {
        for (k, v) in container_env {
            env.insert(k.clone(), substitute_variables(v, workspace));
        }
    }
    if let Some(ref remote_env) = config.remote_env {
        for (k, v) in remote_env {
            env.insert(k.clone(), substitute_variables(v, workspace));
        }
    }

    let ports: Vec<PortMapping> = config
        .forward_ports
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|&p| PortMapping {
            host: p,
            container: p,
        })
        .collect();

    // Resolve the effective remote user from config or image metadata.
    let effective_user = resolve_remote_user(
        runtime.as_ref(),
        &final_image,
        config.remote_user.as_deref(),
    ).await?;
    let remote_user = effective_user.as_deref();
    let mounts: Vec<String> = config
        .mounts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    let extra_args: Vec<String> = config
        .run_args
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    // Merge feature-contributed capabilities (Gap 5).
    let caps = merge_feature_capabilities(&ordered_features);

    let container_config = ContainerConfig {
        image: final_image,
        name: name.clone(),
        labels,
        env,
        mounts: parse_mounts(&mounts),
        ports,
        workspace_mount: Some(WorkspaceMount {
            source: workspace.to_path_buf(),
            target: format!("/workspaces/{folder_name}"),
        }),
        extra_args,
        entrypoint: None,
        init: caps.init,
        privileged: caps.privileged,
        cap_add: caps.cap_add,
        security_opt: caps.security_opt,
    };

    if !container_config.mounts.is_empty() {
        eprintln!(
            "Mounting {} bind mount(s)...",
            container_config.mounts.len()
        );
    }

    eprintln!("Creating container '{name}'...");
    let container_id = runtime.create_container(&container_config).await?;

    eprintln!("Starting container '{name}'...");
    runtime.start_container(&container_id).await?;

    // Run lifecycle hooks — feature hooks first, then config hooks (Gap 6).
    let feature_hooks = if ordered_features.is_empty() {
        None
    } else {
        Some(ordered_features.as_slice())
    };
    run_lifecycle_hooks(runtime.as_ref(), &container_id, &config, remote_user, feature_hooks).await?;

    // Clone dotfiles if configured (Gap 15).
    if let Some(ref dotfiles) = config.dotfiles {
        install_dotfiles(runtime.as_ref(), &container_id, dotfiles, remote_user).await?;
    }

    println!("Container '{name}' is ready.");
    Ok(())
}

/// Run the `initializeCommand` on the host machine (Gap 9).
async fn run_initialize_command(
    cmd: &crate::devcontainer::config::LifecycleCommand,
    workspace: &Path,
) -> anyhow::Result<()> {
    use crate::devcontainer::config::LifecycleCommand;

    async fn run_one(command: &str, workspace: &Path) -> anyhow::Result<()> {
        eprintln!("[lifecycle] Running initializeCommand: {command}");
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(workspace)
            .status()
            .await?;
        if !output.success() {
            anyhow::bail!(
                "initializeCommand failed (exit {}): {command}",
                output.code().unwrap_or(-1)
            );
        }
        Ok(())
    }

    match cmd {
        LifecycleCommand::Single(command) => {
            run_one(command, workspace).await?;
        }
        LifecycleCommand::Multiple(commands) => {
            for command in commands {
                run_one(command, workspace).await?;
            }
        }
        LifecycleCommand::Parallel(commands) => {
            for (_label, command) in commands {
                run_one(command, workspace).await?;
            }
        }
    }

    Ok(())
}

/// Clone and install dotfiles in the container (Gap 15).
async fn install_dotfiles(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    dotfiles: &crate::devcontainer::config::DotfilesConfig,
    user: Option<&str>,
) -> anyhow::Result<()> {
    let target = dotfiles
        .target_path
        .as_deref()
        .unwrap_or("~/dotfiles");

    eprintln!("Cloning dotfiles from {}...", dotfiles.repository);

    // Clone the dotfiles repo
    let clone_cmd = format!(
        "git clone --depth 1 '{}' '{}'",
        dotfiles.repository.replace('\'', "'\\''"),
        target.replace('\'', "'\\''"),
    );
    let args = vec!["sh".to_string(), "-c".to_string(), clone_cmd];
    let result = runtime.exec(container_id, &args, user).await?;
    if result.exit_code != 0 {
        eprintln!(
            "Warning: failed to clone dotfiles (exit {}):\n{}",
            result.exit_code, result.stderr
        );
        return Ok(());
    }

    // Run the install command if specified
    if let Some(ref install_cmd) = dotfiles.install_command {
        eprintln!("Running dotfiles install command: {install_cmd}");
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            install_cmd.clone(),
        ];
        let result = runtime.exec(container_id, &args, user).await?;
        if result.exit_code != 0 {
            eprintln!(
                "Warning: dotfiles install command failed (exit {}):\n{}",
                result.exit_code, result.stderr
            );
        }
    }

    Ok(())
}

/// Parse mount strings from devcontainer.json into `BindMount` structs.
///
/// Supports two formats:
/// - Docker long form: `source=X,target=Y,type=bind[,readonly]`
/// - Docker short form: `/host:/container[:ro]`
fn parse_mounts(mount_strings: &[String]) -> Vec<BindMount> {
    let mut mounts = Vec::new();
    for s in mount_strings {
        if let Some(m) = parse_single_mount(s) {
            mounts.push(m);
        } else {
            eprintln!("Warning: could not parse mount string: {s}");
        }
    }
    mounts
}

fn parse_single_mount(s: &str) -> Option<BindMount> {
    let s = s.trim();

    // Short form: /host:/container[:ro]
    if s.starts_with('/') || s.starts_with('.') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() >= 2 {
            let readonly = parts.get(2).map(|&p| p == "ro").unwrap_or(false);
            return Some(BindMount {
                source: PathBuf::from(parts[0]),
                target: parts[1].to_string(),
                readonly,
            });
        }
        return None;
    }

    // Long form: key=value pairs separated by commas
    let mut source = None;
    let mut target = None;
    let mut readonly = false;

    for part in s.split(',') {
        let part = part.trim();
        if let Some((key, val)) = part.split_once('=') {
            match key {
                "source" | "src" => source = Some(val.to_string()),
                "target" | "dst" | "destination" => target = Some(val.to_string()),
                "readonly" | "ro" => {
                    readonly = val.is_empty() || val == "true" || val == "1";
                }
                "type" => {} // Acknowledged but we only support bind mounts in this context
                _ => {}
            }
        } else if part == "readonly" || part == "ro" {
            readonly = true;
        }
    }

    match (source, target) {
        (Some(src), Some(tgt)) => Some(BindMount {
            source: PathBuf::from(src),
            target: tgt,
            readonly,
        }),
        _ => None,
    }
}
