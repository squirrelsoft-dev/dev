use std::path::Path;

use crate::devcontainer::{
    DevcontainerConfig, download_features, generate_feature_dockerfile, resolve_features,
    stage_feature_context,
};
use crate::devcontainer::features::order_features;
use crate::runtime::detect_runtime;
use crate::util::{container_name, find_devcontainer_config};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    tag: Option<&str>,
    no_cache: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let config_path = find_devcontainer_config(workspace)?;
    let config = DevcontainerConfig::from_path(&config_path)?;
    let runtime = detect_runtime(runtime_override).await?;

    let default_tag = format!("dev-build-{}", container_name(workspace));
    let final_tag = tag.unwrap_or(&default_tag);

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
        // If no features, build directly with the final tag
        let mut features = resolve_features(&config)?;
        if features.is_empty() {
            runtime
                .build_image(&dockerfile_content, &context_dir, final_tag, no_cache, verbose)
                .await?;
            println!("{final_tag}");
            return Ok(());
        }
        // Otherwise build base with a temporary tag
        let base_tag = format!("{final_tag}-base");
        runtime
            .build_image(&dockerfile_content, &context_dir, &base_tag, no_cache, verbose)
            .await?;
        // Fall through to feature layering below
        eprintln!("Downloading {} feature(s)...", features.len());
        download_features(&mut features).await?;
        let ordered = order_features(&features);
        let staging_dir = stage_feature_context(&ordered)?;
        let dockerfile = generate_feature_dockerfile(&base_tag, &ordered, config.remote_user.as_deref());
        eprintln!("Building features image...");
        let result = runtime
            .build_image(&dockerfile, &staging_dir, final_tag, no_cache, verbose)
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result?;
        println!("{final_tag}");
        return Ok(());
    } else {
        anyhow::bail!("devcontainer.json must specify either 'image' or 'build.dockerfile'");
    };

    // Image-based config with features
    let mut features = resolve_features(&config)?;
    if features.is_empty() {
        println!("{base_image}");
        return Ok(());
    }

    eprintln!("Downloading {} feature(s)...", features.len());
    download_features(&mut features).await?;
    let ordered = order_features(&features);
    let staging_dir = stage_feature_context(&ordered)?;
    let dockerfile = generate_feature_dockerfile(&base_image, &ordered, config.remote_user.as_deref());
    eprintln!("Building features image...");
    let result = runtime
        .build_image(&dockerfile, &staging_dir, final_tag, no_cache, verbose)
        .await;
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;

    println!("{final_tag}");
    Ok(())
}
