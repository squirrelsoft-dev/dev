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
use crate::devcontainer::uid;
use crate::runtime::{
    BindMount, ContainerConfig, ContainerRuntime, ContainerState, PortMapping, VolumeMount,
    WorkspaceMount, detect_runtime, resolve_remote_user,
};
use crate::util::{
    container_name, find_config_source, workspace_folder_name, workspace_labels, ConfigSource,
};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    rebuild: bool,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    _buildkit: bool,
    update_remote_user_uid_default: &str,
    port_overrides: &[String],
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

    // Docker Compose configs take a completely separate code path.
    if config.is_compose() {
        return run_compose(
            workspace, &config, &config_path, runtime.as_ref(),
            rebuild, no_cache, verbose, frozen_lockfile,
            update_remote_user_uid_default, port_overrides,
        ).await;
    }

    // Run initializeCommand on the host before anything else (Gap 9).
    if let Some(ref init_cmd) = config.initialize_command {
        run_initialize_command(init_cmd, workspace).await?;
    }

    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut existing = runtime.list_containers(&filters).await?;

    // Fallback: search by local_folder only for containers without config_file label.
    // This matches the official CLI's two-step lookup for backward compatibility.
    if existing.is_empty() && labels_list.len() > 1 {
        let fallback_filter = vec![format!("{}={}", labels_list[0].0, labels_list[0].1)];
        let fallback = runtime.list_containers(&fallback_filter).await?;
        for container in fallback {
            if !container.labels.contains_key("devcontainer.config_file") {
                existing.push(container);
            }
        }
    }

    // Handle existing container.
    // Port bindings are fixed at container creation time, so when --ports
    // is supplied we must recreate the container to apply the new mappings.
    let has_port_overrides = !port_overrides.is_empty();
    if let Some(container) = existing.first() {
        match container.state {
            ContainerState::Running if !rebuild && !has_port_overrides => {
                println!("Container '{}' is already running.", container.name);
                return Ok(());
            }
            ContainerState::Stopped if !rebuild && !has_port_overrides => {
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
                // Rebuild or port override: remove existing
                if has_port_overrides && !rebuild {
                    eprintln!("Recreating container '{}' to apply port overrides...", container.name);
                }
                if rebuild {
                    eprintln!("Removing existing container '{}'...", container.name);
                }
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
    let folder_image = container_name(workspace);
    let final_tag = if has_features {
        format!("{folder_image}-features")
    } else {
        folder_image.clone()
    };

    // Resolve the .devcontainer directory for local feature paths and lockfile.
    let devcontainer_dir: Option<PathBuf> = config_path.parent().map(|p| p.to_path_buf());

    // Track ordered features for later use (capabilities, lifecycle hooks).
    let mut ordered_features = Vec::new();

    let final_image = if !needs_build {
        // Image-based config with no features — use the remote image directly.
        let image = config.image.as_ref()
            .ok_or_else(|| anyhow::anyhow!("devcontainer.json must specify 'image', 'build.dockerfile', or 'dockerComposeFile'"))?;
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
                    .build_image(&dockerfile_content, &context_dir, &final_tag, &HashMap::new(), no_cache, verbose)
                    .await?;
                final_tag.clone()
            } else {
                eprintln!("Building image from Dockerfile...");
                runtime
                    .build_image(&dockerfile_content, &context_dir, &folder_image, &HashMap::new(), no_cache, verbose)
                    .await?;
                folder_image.clone()
            }
        } else {
            anyhow::bail!("devcontainer.json must specify 'image', 'build.dockerfile', or 'dockerComposeFile'");
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
            );
            if verbose {
                eprintln!("Features Dockerfile:\n{dockerfile}");
            }
            eprintln!("Building features image...");
            let result = runtime
                .build_image(&dockerfile, &staging_dir, &final_tag, &HashMap::new(), no_cache, verbose)
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
    for (k, v) in &labels_list {
        labels.insert(k.clone(), v.clone());
    }

    // Substitute devcontainer variables in env values
    let mut env = HashMap::new();
    env.insert("REMOTE_CONTAINERS".to_string(), "true".to_string());
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

    let ports: Vec<PortMapping> = if port_overrides.is_empty() {
        config.forward_ports.clone().unwrap_or_default()
    } else {
        parse_port_overrides(port_overrides)?
    };
    let caddy_host_ports: Vec<crate::caddy::PortEntry> = ports
        .iter()
        .map(|p| crate::caddy::PortEntry { port: p.host, custom_name: None })
        .collect();

    // Resolve the effective remote user from config or image metadata.
    let effective_user = resolve_remote_user(
        runtime.as_ref(),
        &final_image,
        config.remote_user.as_deref(),
    ).await?;
    let remote_user = effective_user.as_deref();

    // Optionally build a UID-remapping layer to match host UID/GID.
    let final_image = if uid::should_remap_uid(&config, remote_user, update_remote_user_uid_default) {
        let image_meta = runtime.inspect_image_metadata(&final_image).await?;
        let image_user = image_meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(
            runtime.as_ref(),
            &final_image,
            &folder_image,
            remote_user.unwrap_or("root"),
            image_user,
            no_cache,
            verbose,
        ).await?
    } else {
        final_image
    };

    let mounts: Vec<String> = config
        .mounts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    let volume_strings: Vec<String> = config
        .volumes
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();
    let volumes = parse_volumes(&volume_strings);

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
        volumes,
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

    if !caddy_host_ports.is_empty() {
        if let Err(e) = crate::caddy::register_site(workspace, &caddy_host_ports) {
            eprintln!("Warning: Caddy setup failed: {e}");
        }
    }

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

/// Handle a Docker Compose-based devcontainer config.
///
/// Full pipeline: build the service, layer features on top, UID-remap,
/// generate a compose override injecting labels/env/mounts/ports/image,
/// start services, run lifecycle hooks, install dotfiles.
#[allow(clippy::too_many_arguments)]
async fn run_compose(
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
    runtime: &dyn ContainerRuntime,
    _rebuild: bool,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    update_remote_user_uid_default: &str,
    port_overrides: &[String],
) -> anyhow::Result<()> {
    let compose_data = config.docker_compose_file.as_ref().unwrap();
    let compose_files = compose_data.files();
    let devcontainer_dir = config_path.parent().unwrap();
    let devcontainer_dir_buf: Option<PathBuf> = Some(devcontainer_dir.to_path_buf());
    let service = config.service.as_deref()
        .ok_or_else(|| anyhow::anyhow!("Docker Compose config must specify 'service'"))?;
    let project_name = container_name(workspace);
    let folder_image = container_name(workspace);
    let runtime_name = runtime.runtime_name();

    // Workspace-related env vars for Docker Compose variable interpolation.
    // Compose files use ${localWorkspaceFolder}, ${localWorkspaceFolderBasename},
    // etc. in volume paths and other settings. These must be set as process env
    // vars so `docker compose` resolves them when parsing the compose file.
    let folder_name = workspace_folder_name(workspace);
    let workspace_source = workspace.canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let workspace_target = substitute_variables(
        config.workspace_folder.as_deref()
            .unwrap_or(&format!("/workspaces/{folder_name}")),
        workspace,
    );
    let mut compose_env = HashMap::new();
    compose_env.insert(
        "localWorkspaceFolder".to_string(),
        workspace_source.to_string_lossy().to_string(),
    );
    compose_env.insert(
        "localWorkspaceFolderBasename".to_string(),
        folder_name.clone(),
    );
    compose_env.insert(
        "containerWorkspaceFolder".to_string(),
        workspace_target.clone(),
    );

    // 1. initializeCommand
    if let Some(ref init_cmd) = config.initialize_command {
        run_initialize_command(init_cmd, workspace).await?;
    }

    // 2. Always build the service (features need the base image).
    eprintln!("Building compose services...");
    crate::runtime::compose::compose_build(
        runtime_name, &compose_files, devcontainer_dir,
        Some(service), no_cache, verbose, &compose_env,
    ).await?;

    // 3. Get the built service image name.
    let base_image = crate::runtime::compose::compose_service_image(
        runtime_name, &compose_files, devcontainer_dir,
        &project_name, service, &compose_env,
    ).await?;
    if verbose {
        eprintln!("Service image: {base_image}");
    }

    // 4. Feature pipeline.
    let initial_features = resolve_features(config)?;
    let has_features = !initial_features.is_empty();
    let mut ordered_features = Vec::new();

    let featured_image = if has_features {
        let mut features = initial_features;
        let original_count = features.len();
        eprintln!("Downloading {} feature(s)...", original_count);
        if verbose {
            for f in &features {
                eprintln!("  Feature: {} ({}:{})", f.id, f.oci_ref, f.version);
            }
        }
        download_features(&mut features, devcontainer_dir_buf.as_deref()).await?;

        if features.len() > original_count {
            eprintln!(
                "Resolved {} transitive dependencies",
                features.len() - original_count
            );
        }

        // Lockfile handling.
        if let Some(ref dc_dir) = devcontainer_dir_buf {
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
            runtime, &base_image, config.remote_user.as_deref(),
        ).await?;
        let feature_tag = format!("{folder_image}-features");
        let dockerfile = generate_feature_dockerfile_with_opts(
            &base_image, &ordered, feature_user.as_deref(), config,
        );
        if verbose {
            eprintln!("Features Dockerfile:\n{dockerfile}");
        }
        eprintln!("Building features image...");
        let result = runtime
            .build_image(&dockerfile, &staging_dir, &feature_tag, &HashMap::new(), no_cache, verbose)
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result.map_err(|e| anyhow::anyhow!("{e}"))?;

        ordered_features = ordered;
        feature_tag
    } else {
        base_image.clone()
    };

    // 5. Resolve remote user from the final image.
    let effective_user = resolve_remote_user(
        runtime, &featured_image, config.remote_user.as_deref(),
    ).await?;
    let remote_user = effective_user.as_deref();

    // 6. UID remapping.
    let final_image = if uid::should_remap_uid(config, remote_user, update_remote_user_uid_default) {
        let image_meta = runtime.inspect_image_metadata(&featured_image).await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let image_user = image_meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(
            runtime, &featured_image, &folder_image,
            remote_user.unwrap_or("root"), image_user,
            no_cache, verbose,
        ).await?
    } else {
        featured_image
    };

    let image_override = if final_image != base_image {
        Some(final_image.as_str())
    } else {
        None
    };

    // 7. Variable substitution on env, mounts, volumes.
    let mut env = HashMap::new();
    env.insert("REMOTE_CONTAINERS".to_string(), "true".to_string());
    if let Some(ref container_env) = config.container_env {
        for (k, v) in container_env {
            env.insert(k.clone(), substitute_variables_with_user(v, workspace, remote_user));
        }
    }
    if let Some(ref remote_env) = config.remote_env {
        for (k, v) in remote_env {
            env.insert(k.clone(), substitute_variables_with_user(v, workspace, remote_user));
        }
    }

    let mounts: Vec<String> = config
        .mounts.as_deref().unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    let volume_strings: Vec<String> = config
        .volumes.as_deref().unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    let ports: Vec<PortMapping> = if port_overrides.is_empty() {
        config.forward_ports.clone().unwrap_or_default()
    } else {
        parse_port_overrides(port_overrides)?
    };
    let caddy_host_ports_compose: Vec<crate::caddy::PortEntry> = ports
        .iter()
        .map(|p| crate::caddy::PortEntry { port: p.host, custom_name: None })
        .collect();

    // 8. Labels + merged feature capabilities.
    let labels_list = workspace_labels(workspace, Some(config_path));
    let caps = merge_feature_capabilities(&ordered_features);

    // 9. Generate and write compose override file.
    let override_content = crate::runtime::compose::generate_compose_override(
        service, &labels_list, &env, &mounts, &volume_strings,
        &ports, image_override, &caps,
    );
    let override_path = crate::runtime::compose::write_override_file(&override_content)?;
    let override_path_str = override_path.to_string_lossy().to_string();
    if verbose {
        eprintln!("Compose override:\n{override_content}");
    }

    // 10. Rewrite compose file volume sources so `..` resolves to the actual
    //     workspace instead of ~/.dev/devcontainers/. Use rewritten files for
    //     compose_up (not compose_build, which needs original paths for Dockerfiles).
    let mut rewritten_paths = Vec::new();
    let mut up_files: Vec<String> = Vec::new();
    for f in &compose_files {
        let compose_path = if Path::new(f).is_absolute() {
            PathBuf::from(f)
        } else {
            devcontainer_dir.join(f)
        };
        match crate::runtime::compose::rewrite_compose_volumes(&compose_path, workspace) {
            Ok(rewritten) => {
                up_files.push(rewritten.to_string_lossy().to_string());
                rewritten_paths.push(rewritten);
            }
            Err(_) => {
                // Fall back to original if rewrite fails.
                up_files.push(compose_path.to_string_lossy().to_string());
            }
        }
    }
    up_files.push(override_path_str.clone());
    let up_file_refs: Vec<&str> = up_files.iter().map(|s| s.as_str()).collect();

    eprintln!("Starting compose services...");
    crate::runtime::compose::compose_up(
        runtime_name, &up_file_refs, devcontainer_dir,
        &project_name, &compose_env, verbose,
    ).await?;

    // 11. Get container ID.
    let container_id = crate::runtime::compose::compose_container_id(
        runtime_name, &up_file_refs, devcontainer_dir,
        &project_name, service,
    ).await?;

    // 12. Run lifecycle hooks with feature hooks and correct remote_user.
    let feature_hooks = if ordered_features.is_empty() {
        None
    } else {
        Some(ordered_features.as_slice())
    };
    run_lifecycle_hooks(runtime, &container_id, config, remote_user, feature_hooks).await?;

    // 13. Install dotfiles.
    if let Some(ref dotfiles) = config.dotfiles {
        install_dotfiles(runtime, &container_id, dotfiles, remote_user).await?;
    }

    // Cleanup temp files.
    let _ = std::fs::remove_file(&override_path);
    for p in &rewritten_paths {
        let _ = std::fs::remove_file(p);
    }

    println!("Compose service '{service}' is ready (container {}).", &container_id[..12.min(container_id.len())]);

    if !caddy_host_ports_compose.is_empty() {
        if let Err(e) = crate::caddy::register_site(workspace, &caddy_host_ports_compose) {
            eprintln!("Warning: Caddy setup failed: {e}");
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

/// Parse CLI `--ports` values into `PortMapping` structs.
///
/// Accepted formats:
/// - `8080` — forward container port 8080 to host port 8080
/// - `9090:8080` — forward container port 8080 to host port 9090
fn parse_port_overrides(args: &[String]) -> anyhow::Result<Vec<PortMapping>> {
    let mut mappings = Vec::new();
    for arg in args {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if let Some((host_str, container_str)) = arg.split_once(':') {
            let host: u16 = host_str
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid host port in '{arg}'"))?;
            let container: u16 = container_str
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid container port in '{arg}'"))?;
            mappings.push(PortMapping { host, container });
        } else {
            let port: u16 = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid port '{arg}'"))?;
            mappings.push(PortMapping {
                host: port,
                container: port,
            });
        }
    }
    Ok(mappings)
}

/// Parse volume strings into `VolumeMount` structs.
///
/// Format: `volume-name:/container/path[:ro]`
fn parse_volumes(volume_strings: &[String]) -> Vec<VolumeMount> {
    let mut volumes = Vec::new();
    for s in volume_strings {
        let s = s.trim();
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() >= 2 {
            let readonly = parts.get(2).is_some_and(|&p| p == "ro");
            volumes.push(VolumeMount {
                name: parts[0].to_string(),
                target: parts[1].to_string(),
                readonly,
            });
        } else {
            eprintln!("Warning: could not parse volume string (expected name:/path[:ro]): {s}");
        }
    }
    volumes
}
