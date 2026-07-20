use std::path::Path;

use crate::devcontainer::compose::compose_recipe_config;
use crate::devcontainer::effective::{
    LockfilePolicy, effective_config_from_parts, load_effective_config,
};
use crate::devcontainer::features::{
    feature_image_tag, generate_feature_dockerfile_with_opts, order_features,
};
use crate::devcontainer::substitute_variables;
use crate::devcontainer::uid;
use crate::devcontainer::{Recipe, download_features, resolve_features, stage_feature_context};
use crate::runtime::{detect_runtime, resolve_remote_user};
use crate::util::{ConfigSource, container_name, find_config_source, workspace_folder_name};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    tag: Option<&str>,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    _buildkit: bool,
    update_remote_user_uid_default: &str,
    no_base: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let (config_path, recipe_config) = match find_config_source(workspace)? {
        ConfigSource::Direct(path) => (path, None),
        ConfigSource::Recipe(recipe_path) => {
            let recipe = Recipe::from_path(&recipe_path)?;
            let composed =
                compose_recipe_config(&recipe_path, &recipe, runtime.runtime_name(), !no_base)?;
            (composed.config_path.clone(), Some(composed))
        }
    };
    // Recipe configs resolve their own layers, base included, so the runtime base
    // layer would be a second application whose prune can discard a base selector
    // the recipe deliberately kept.
    let effective = match recipe_config {
        Some(recipe_config) => {
            effective_config_from_parts(recipe_config.value, recipe_config.base_feature_ids, true)?
        }
        None => load_effective_config(&config_path, !no_base)?,
    };
    let lockfile = LockfilePolicy::new(&effective, frozen_lockfile);
    let config = effective.config;

    let folder_image = container_name(workspace);
    let features = resolve_features(&config)?;
    let has_features = !features.is_empty();
    let default_tag = if has_features {
        feature_image_tag(&folder_image, &config, &features)
    } else {
        folder_image.clone()
    };
    let final_tag = tag.unwrap_or(&default_tag);
    let devcontainer_dir = config_path.parent().map(|p| p.to_path_buf());

    // Build or pull the base image
    let base_image = if let Some(ref image) = config.image {
        eprintln!("Pulling image '{image}'...");
        runtime.pull_image(image).await?;
        image.clone()
    } else if let Some(ref build) = config.build {
        let context_dir = config_path
            .parent()
            .unwrap()
            .join(build.context.as_deref().unwrap_or("."));
        let dockerfile_path = config_path.parent().unwrap().join(&build.dockerfile);
        let dockerfile_content = std::fs::read_to_string(&dockerfile_path)?;
        eprintln!("Building image from Dockerfile...");
        if !has_features {
            // No features — build directly with the final tag.
            runtime
                .build_image(
                    &dockerfile_content,
                    &context_dir,
                    final_tag,
                    &std::collections::HashMap::new(),
                    no_cache,
                    verbose,
                )
                .await?;
            let remote_user =
                resolve_remote_user(runtime.as_ref(), final_tag, config.remote_user.as_deref())
                    .await?;
            let output_tag = if uid::should_remap_uid(
                &config,
                remote_user.as_deref(),
                update_remote_user_uid_default,
            ) {
                let meta = runtime.inspect_image_metadata(final_tag).await?;
                let image_user = meta.container_user.as_deref().unwrap_or("root");
                uid::build_uid_image(
                    runtime.as_ref(),
                    final_tag,
                    &folder_image,
                    remote_user.as_deref().unwrap_or("root"),
                    image_user,
                    no_cache,
                    verbose,
                )
                .await?
            } else {
                final_tag.to_string()
            };
            println!("{output_tag}");
            return Ok(());
        }
        // Features present: tag the base Dockerfile build as the folder image.
        runtime
            .build_image(
                &dockerfile_content,
                &context_dir,
                &folder_image,
                &std::collections::HashMap::new(),
                no_cache,
                verbose,
            )
            .await?;
        // Fall through to feature layering below
        let mut features = features;
        eprintln!("Downloading {} feature(s)...", features.len());
        download_features(&mut features, devcontainer_dir.as_deref()).await?;

        lockfile.apply(devcontainer_dir.as_deref(), &features)?;

        let ordered = order_features(&features);
        let staging_dir = stage_feature_context(&ordered)?;
        let feature_user = resolve_remote_user(
            runtime.as_ref(),
            &folder_image,
            config.remote_user.as_deref(),
        )
        .await?;
        let dockerfile = generate_feature_dockerfile_with_opts(
            &folder_image,
            &ordered,
            feature_user.as_deref(),
            &config,
        );
        eprintln!("Building features image...");
        let result = runtime
            .build_image(
                &dockerfile,
                &staging_dir,
                final_tag,
                &std::collections::HashMap::new(),
                no_cache,
                verbose,
            )
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result?;
        let output_tag = if uid::should_remap_uid(
            &config,
            feature_user.as_deref(),
            update_remote_user_uid_default,
        ) {
            let meta = runtime.inspect_image_metadata(final_tag).await?;
            let image_user = meta.container_user.as_deref().unwrap_or("root");
            uid::build_uid_image(
                runtime.as_ref(),
                final_tag,
                &folder_image,
                feature_user.as_deref().unwrap_or("root"),
                image_user,
                no_cache,
                verbose,
            )
            .await?
        } else {
            final_tag.to_string()
        };
        println!("{output_tag}");
        return Ok(());
    } else if config.is_compose() {
        let compose_data = config.docker_compose_file.as_ref().unwrap();
        let compose_files = compose_data.files();
        let compose_devcontainer_dir = config_path.parent().unwrap();
        let service = config
            .service
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Docker Compose config must specify 'service'"))?;
        let project_name = container_name(workspace);

        // Workspace env vars for Docker Compose variable interpolation.
        let build_folder_name = workspace_folder_name(workspace);
        let build_workspace_source = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let build_workspace_target = substitute_variables(
            config
                .workspace_folder
                .as_deref()
                .unwrap_or(&format!("/workspaces/{build_folder_name}")),
            workspace,
        );
        let mut compose_env = std::collections::HashMap::new();
        compose_env.insert(
            "localWorkspaceFolder".to_string(),
            build_workspace_source.to_string_lossy().to_string(),
        );
        compose_env.insert(
            "localWorkspaceFolderBasename".to_string(),
            build_folder_name,
        );
        compose_env.insert(
            "containerWorkspaceFolder".to_string(),
            build_workspace_target,
        );

        eprintln!("Building compose services...");
        crate::runtime::compose::compose_build(
            runtime.runtime_name(),
            &compose_files,
            compose_devcontainer_dir,
            Some(service),
            no_cache,
            verbose,
            &compose_env,
        )
        .await?;

        if !has_features {
            println!("compose:{service}");
            return Ok(());
        }

        // Get the service image for feature layering.
        let base_image = crate::runtime::compose::compose_service_image(
            runtime.runtime_name(),
            &compose_files,
            compose_devcontainer_dir,
            &project_name,
            service,
            &compose_env,
        )
        .await?;

        let feature_tag = feature_image_tag(&folder_image, &config, &features);
        let compose_final_tag = tag.unwrap_or(&feature_tag);

        let mut features = features;
        eprintln!("Downloading {} feature(s)...", features.len());
        download_features(&mut features, Some(compose_devcontainer_dir)).await?;

        lockfile.apply(Some(compose_devcontainer_dir), &features)?;

        let ordered = order_features(&features);
        let staging_dir = stage_feature_context(&ordered)?;
        let feature_user =
            resolve_remote_user(runtime.as_ref(), &base_image, config.remote_user.as_deref())
                .await?;
        let dockerfile = generate_feature_dockerfile_with_opts(
            &base_image,
            &ordered,
            feature_user.as_deref(),
            &config,
        );
        eprintln!("Building features image...");
        let result = runtime
            .build_image(
                &dockerfile,
                &staging_dir,
                compose_final_tag,
                &std::collections::HashMap::new(),
                no_cache,
                verbose,
            )
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result?;

        let output_tag = if uid::should_remap_uid(
            &config,
            feature_user.as_deref(),
            update_remote_user_uid_default,
        ) {
            let meta = runtime.inspect_image_metadata(compose_final_tag).await?;
            let image_user = meta.container_user.as_deref().unwrap_or("root");
            uid::build_uid_image(
                runtime.as_ref(),
                compose_final_tag,
                &folder_image,
                feature_user.as_deref().unwrap_or("root"),
                image_user,
                no_cache,
                verbose,
            )
            .await?
        } else {
            compose_final_tag.to_string()
        };
        println!("{output_tag}");
        return Ok(());
    } else {
        anyhow::bail!(
            "devcontainer.json must specify 'image', 'build.dockerfile', or 'dockerComposeFile'"
        );
    };

    // Image-based config with features
    if !has_features {
        println!("{base_image}");
        return Ok(());
    }

    let mut features = features;
    eprintln!("Downloading {} feature(s)...", features.len());
    download_features(&mut features, devcontainer_dir.as_deref()).await?;

    lockfile.apply(devcontainer_dir.as_deref(), &features)?;

    let ordered = order_features(&features);
    let staging_dir = stage_feature_context(&ordered)?;
    let feature_user =
        resolve_remote_user(runtime.as_ref(), &base_image, config.remote_user.as_deref()).await?;
    let dockerfile = generate_feature_dockerfile_with_opts(
        &base_image,
        &ordered,
        feature_user.as_deref(),
        &config,
    );
    eprintln!("Building features image...");
    let result = runtime
        .build_image(
            &dockerfile,
            &staging_dir,
            final_tag,
            &std::collections::HashMap::new(),
            no_cache,
            verbose,
        )
        .await;
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;

    let output_tag = if uid::should_remap_uid(
        &config,
        feature_user.as_deref(),
        update_remote_user_uid_default,
    ) {
        let meta = runtime.inspect_image_metadata(final_tag).await?;
        let image_user = meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(
            runtime.as_ref(),
            final_tag,
            &folder_image,
            feature_user.as_deref().unwrap_or("root"),
            image_user,
            no_cache,
            verbose,
        )
        .await?
    } else {
        final_tag.to_string()
    };
    println!("{output_tag}");
    Ok(())
}
