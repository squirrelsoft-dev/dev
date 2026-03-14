use std::path::Path;

use crate::devcontainer::{
    DevcontainerConfig, Recipe, compose_and_write, download_features,
    resolve_features, stage_feature_context,
};
use crate::devcontainer::features::{generate_feature_dockerfile_with_opts, order_features};
use crate::devcontainer::lockfile::{handle_lockfile, lockfile_path};
use crate::devcontainer::uid;
use crate::runtime::{detect_runtime, resolve_remote_user};
use crate::util::{container_name, find_config_source, ConfigSource};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    tag: Option<&str>,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    buildkit: bool,
    update_remote_user_uid_default: &str,
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

    let folder_image = container_name(workspace);
    let features = resolve_features(&config)?;
    let has_features = !features.is_empty();
    let default_tag = if has_features {
        format!("{folder_image}-features")
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
        let dockerfile_path = config_path
            .parent()
            .unwrap()
            .join(&build.dockerfile);
        let dockerfile_content = std::fs::read_to_string(&dockerfile_path)?;
        eprintln!("Building image from Dockerfile...");
        if !has_features {
            // No features — build directly with the final tag.
            runtime
                .build_image(&dockerfile_content, &context_dir, final_tag, &std::collections::HashMap::new(), no_cache, verbose)
                .await?;
            let remote_user = resolve_remote_user(runtime.as_ref(), final_tag, config.remote_user.as_deref()).await?;
            let output_tag = if uid::should_remap_uid(&config, remote_user.as_deref(), update_remote_user_uid_default) {
                let meta = runtime.inspect_image_metadata(final_tag).await?;
                let image_user = meta.container_user.as_deref().unwrap_or("root");
                uid::build_uid_image(runtime.as_ref(), final_tag, &folder_image, remote_user.as_deref().unwrap_or("root"), image_user, no_cache, verbose).await?
            } else {
                final_tag.to_string()
            };
            println!("{output_tag}");
            return Ok(());
        }
        // Features present: tag the base Dockerfile build as the folder image.
        runtime
            .build_image(&dockerfile_content, &context_dir, &folder_image, &std::collections::HashMap::new(), no_cache, verbose)
            .await?;
        // Fall through to feature layering below
        let mut features = features;
        eprintln!("Downloading {} feature(s)...", features.len());
        download_features(&mut features, devcontainer_dir.as_deref()).await?;

        if let Some(ref dc_dir) = devcontainer_dir {
            handle_lockfile(&lockfile_path(dc_dir), &features, frozen_lockfile)?;
        }

        let ordered = order_features(&features);
        let staging_dir = stage_feature_context(&ordered)?;
        let feature_user = resolve_remote_user(
            runtime.as_ref(),
            &folder_image,
            config.remote_user.as_deref(),
        ).await?;
        let dockerfile = generate_feature_dockerfile_with_opts(&folder_image, &ordered, feature_user.as_deref(), &config, buildkit);
        eprintln!("Building features image...");
        let result = runtime
            .build_image(&dockerfile, &staging_dir, final_tag, &std::collections::HashMap::new(), no_cache, verbose)
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result?;
        let output_tag = if uid::should_remap_uid(&config, feature_user.as_deref(), update_remote_user_uid_default) {
            let meta = runtime.inspect_image_metadata(final_tag).await?;
            let image_user = meta.container_user.as_deref().unwrap_or("root");
            uid::build_uid_image(runtime.as_ref(), final_tag, &folder_image, feature_user.as_deref().unwrap_or("root"), image_user, no_cache, verbose).await?
        } else {
            final_tag.to_string()
        };
        println!("{output_tag}");
        return Ok(());
    } else {
        anyhow::bail!("devcontainer.json must specify either 'image' or 'build.dockerfile'");
    };

    // Image-based config with features
    if !has_features {
        println!("{base_image}");
        return Ok(());
    }

    let mut features = features;
    eprintln!("Downloading {} feature(s)...", features.len());
    download_features(&mut features, devcontainer_dir.as_deref()).await?;

    if let Some(ref dc_dir) = devcontainer_dir {
        handle_lockfile(&lockfile_path(dc_dir), &features, frozen_lockfile)?;
    }

    let ordered = order_features(&features);
    let staging_dir = stage_feature_context(&ordered)?;
    let feature_user = resolve_remote_user(
        runtime.as_ref(),
        &base_image,
        config.remote_user.as_deref(),
    ).await?;
    let dockerfile = generate_feature_dockerfile_with_opts(&base_image, &ordered, feature_user.as_deref(), &config, buildkit);
    eprintln!("Building features image...");
    let result = runtime
        .build_image(&dockerfile, &staging_dir, final_tag, &std::collections::HashMap::new(), no_cache, verbose)
        .await;
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;

    let output_tag = if uid::should_remap_uid(&config, feature_user.as_deref(), update_remote_user_uid_default) {
        let meta = runtime.inspect_image_metadata(final_tag).await?;
        let image_user = meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(runtime.as_ref(), final_tag, &folder_image, feature_user.as_deref().unwrap_or("root"), image_user, no_cache, verbose).await?
    } else {
        final_tag.to_string()
    };
    println!("{output_tag}");
    Ok(())
}
