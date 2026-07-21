use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::devcontainer::compose::{compose_recipe_config, materialize_recipe_directory};
use crate::devcontainer::config::MountSpec;
use crate::devcontainer::effective::{
    LockfilePolicy, effective_config_from_parts, load_effective_config,
};
use crate::devcontainer::features::{
    MergedCapabilities, ResolvedFeature, capabilities_from_metadata, feature_image_tag,
    generate_feature_dockerfile_with_opts, order_features,
};
use crate::devcontainer::uid;
use crate::devcontainer::{
    DevcontainerConfig, Recipe, download_features, merge_feature_capabilities, resolve_features,
    run_lifecycle_hooks, stage_feature_context, substitute_variables,
    substitute_variables_with_user,
};
use crate::runtime::{
    BindMount, ContainerConfig, ContainerRuntime, ContainerState, ExecResult, PortMapping,
    VolumeMount, WorkspaceMount, detect_runtime, resolve_remote_user,
};
use crate::util::{
    ConfigSource, container_name, find_config_source, workspace_folder_name, workspace_labels,
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
    no_base: bool,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    run_with_runtime(
        workspace,
        runtime.as_ref(),
        rebuild,
        no_cache,
        verbose,
        frozen_lockfile,
        update_remote_user_uid_default,
        port_overrides,
        no_base,
    )
    .await
}

/// `dev up` body once the runtime has been selected.
///
/// Split from [`run`] so the create/start/readiness flow can be driven with a
/// stand-in [`ContainerRuntime`] in tests — the issue #4 regression boundary is
/// that a failed create or start must propagate as an error before any
/// "ready" message, and that the container config handed to the runtime
/// carries the workspace label that `dev status`/`dev exec` later filter on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_runtime(
    workspace: &Path,
    runtime: &dyn ContainerRuntime,
    rebuild: bool,
    no_cache: bool,
    verbose: bool,
    frozen_lockfile: bool,
    update_remote_user_uid_default: &str,
    port_overrides: &[String],
    no_base: bool,
) -> anyhow::Result<()> {
    let (config_path, recipe_config) = match find_config_source(workspace)? {
        ConfigSource::Direct(path) => (path, None),
        ConfigSource::Recipe(recipe_path) => {
            let recipe = Recipe::from_path(&recipe_path)?;
            materialize_recipe_directory(&recipe_path, &recipe)?;
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
            effective_config_from_parts(recipe_config.value, recipe_config.base_feature_ids)?
        }
        None => load_effective_config(&config_path, !no_base)?,
    };
    let lockfile = LockfilePolicy::new(&effective, frozen_lockfile);
    let mut config = effective.config;
    apply_cli_overrides(&mut config, port_overrides)?;

    // Docker Compose configs take a completely separate code path.
    if config.is_compose() {
        return run_compose(
            workspace,
            &config,
            &config_path,
            runtime,
            rebuild,
            no_cache,
            verbose,
            update_remote_user_uid_default,
            &lockfile,
        )
        .await;
    }

    // Run initializeCommand on the host before anything else (Gap 9).
    if let Some(ref init_cmd) = config.initialize_command {
        run_initialize_command(init_cmd, workspace).await?;
    }

    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
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
                // Resolved before the gate, not just for the hooks: the probe
                // certifies the user every later command actually runs as.
                let user =
                    resolve_remote_user(runtime, &container.image, config.remote_user.as_deref())
                        .await?;
                verify_container_usable(runtime, &container.id, workspace, user.as_deref()).await?;
                if config.post_start_command.is_some() {
                    run_lifecycle_hooks(runtime, &container.id, &config, user.as_deref(), None)
                        .await?;
                }
                println!("Container '{}' started.", container.name);
                return Ok(());
            }
            _ => {
                // Rebuild or port override: remove existing
                if has_port_overrides && !rebuild {
                    eprintln!(
                        "Recreating container '{}' to apply port overrides...",
                        container.name
                    );
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
        feature_image_tag(&folder_image, &config, &initial_features)
    } else {
        folder_image.clone()
    };

    // Resolve the .devcontainer directory for local feature paths and lockfile.
    let devcontainer_dir: Option<PathBuf> = config_path.parent().map(|p| p.to_path_buf());

    // Track ordered features for later use (capabilities, lifecycle hooks).
    let mut ordered_features = Vec::new();

    let final_image = if !needs_build {
        // Image-based config with no features — use the image directly. If the
        // image is already present locally, skip the pull (mirrors the reference
        // devcontainer CLI, which inspects the local image before pulling).
        let image = config.image.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "devcontainer.json must specify 'image', 'build.dockerfile', or 'dockerComposeFile'"
            )
        })?;
        ensure_image_present(runtime, image).await?;
        image.clone()
    } else if !rebuild && !no_cache && runtime.image_exists(&final_tag).await? {
        // Image already built (e.g. by `dev build`), skip rebuild.
        eprintln!("Image '{final_tag}' already exists, skipping build.");
        final_tag
    } else {
        // Determine base image
        let base_image = if let Some(ref image) = config.image {
            ensure_image_present(runtime, image).await?;
            image.clone()
        } else if let Some(ref build) = config.build {
            let context_dir = config_path
                .parent()
                .unwrap()
                .join(build.context.as_deref().unwrap_or("."));
            let dockerfile_path = config_path.parent().unwrap().join(&build.dockerfile);
            let dockerfile_content = std::fs::read_to_string(&dockerfile_path)?;
            if !has_features {
                // No features — build directly with the final tag.
                eprintln!("Building image from Dockerfile...");
                runtime
                    .build_image(
                        &dockerfile_content,
                        &context_dir,
                        &final_tag,
                        &HashMap::new(),
                        no_cache,
                        verbose,
                    )
                    .await?;
                final_tag.clone()
            } else {
                eprintln!("Building image from Dockerfile...");
                runtime
                    .build_image(
                        &dockerfile_content,
                        &context_dir,
                        &folder_image,
                        &HashMap::new(),
                        no_cache,
                        verbose,
                    )
                    .await?;
                folder_image.clone()
            }
        } else {
            anyhow::bail!(
                "devcontainer.json must specify 'image', 'build.dockerfile', or 'dockerComposeFile'"
            );
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
            lockfile.apply(devcontainer_dir.as_deref(), &features)?;

            let ordered = order_features(&features);
            if verbose {
                eprintln!("Feature install order:");
                for (i, f) in ordered.iter().enumerate() {
                    eprintln!(
                        "  {}: {}{}",
                        i + 1,
                        f.id,
                        if f.is_dependency { " (dependency)" } else { "" }
                    );
                }
            }
            let staging_dir = stage_feature_context(&ordered)?;
            let feature_user =
                resolve_remote_user(runtime, &base_image, config.remote_user.as_deref()).await?;
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
                .build_image(
                    &dockerfile,
                    &staging_dir,
                    &final_tag,
                    &HashMap::new(),
                    no_cache,
                    verbose,
                )
                .await;
            let _ = std::fs::remove_dir_all(&staging_dir);
            result?;

            ordered_features = ordered;
        }

        final_tag
    };

    // Resolve feature capabilities against the image the features produced, before the
    // UID-remap layer below shadows `final_image` with a derived tag.
    let caps =
        resolve_container_capabilities(runtime, &final_image, &ordered_features, has_features)
            .await?;

    // Build container config
    let name = container_name(workspace);

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

    let ports: Vec<PortMapping> = config.forward_ports.clone().unwrap_or_default();
    let caddy_host_ports: Vec<crate::caddy::PortEntry> = ports
        .iter()
        .map(|p| crate::caddy::PortEntry {
            port: p.host,
            custom_name: None,
            keepalive: None,
        })
        .collect();

    // Resolve the effective remote user from config or image metadata.
    let effective_user =
        resolve_remote_user(runtime, &final_image, config.remote_user.as_deref()).await?;
    let remote_user = effective_user.as_deref();

    // Optionally build a UID-remapping layer to match host UID/GID.
    let final_image = if uid::should_remap_uid(&config, remote_user, update_remote_user_uid_default)
    {
        let image_meta = runtime.inspect_image_metadata(&final_image).await?;
        let image_user = image_meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(
            runtime,
            &final_image,
            &folder_image,
            remote_user.unwrap_or("root"),
            image_user,
            no_cache,
            verbose,
        )
        .await?
    } else {
        final_image
    };

    let mount_strings = substitute_mounts(
        config.mounts.as_deref().unwrap_or(&[]),
        workspace,
        remote_user,
    );
    let mounts = parse_mounts(&mount_strings);

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

    let container_config = ContainerConfig {
        image: final_image,
        name: name.clone(),
        labels,
        env,
        mounts,
        volumes,
        ports,
        workspace_mount: Some(WorkspaceMount {
            source: workspace.to_path_buf(),
            target: config.workspace_mount_target(workspace, remote_user)?,
        }),
        workspace_folder: Some(config.workspace_folder_path(workspace, remote_user)?),
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
    verify_container_usable(runtime, &container_id, workspace, remote_user).await?;

    // Run lifecycle hooks — feature hooks first, then config hooks (Gap 6).
    let feature_hooks = if ordered_features.is_empty() {
        None
    } else {
        Some(ordered_features.as_slice())
    };
    run_lifecycle_hooks(runtime, &container_id, &config, remote_user, feature_hooks).await?;

    // Clone dotfiles if configured (Gap 15).
    if let Some(ref dotfiles) = config.dotfiles {
        install_dotfiles(runtime, &container_id, dotfiles, remote_user).await?;
    }

    println!("Container '{name}' is ready.");

    if !caddy_host_ports.is_empty()
        && let Err(e) = crate::caddy::register_site(workspace, &caddy_host_ports)
    {
        eprintln!("Warning: Caddy setup failed: {e}");
    }

    Ok(())
}

/// How long to give the runtime to report a just-started container as running.
///
/// Generous because a VM-backed runtime boots a guest before the daemon settles
/// on `Running`; the polls back off so a healthy container is still confirmed
/// in the first hundred milliseconds.
const READINESS_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
const READINESS_FIRST_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const READINESS_MAX_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// The retry schedule every part of the readiness gate follows.
///
/// Both halves are asking the same question — is this runtime settled yet? — so
/// both wait the same way. A one-shot check next to a patient one would fail a
/// healthy container on whichever half happened to run first.
struct ReadinessPolls {
    deadline: tokio::time::Instant,
    next: std::time::Duration,
}

impl ReadinessPolls {
    fn new() -> Self {
        Self {
            deadline: tokio::time::Instant::now() + READINESS_BUDGET,
            next: READINESS_FIRST_POLL,
        }
    }

    /// Wait before the next attempt, or report that the budget is spent.
    async fn wait(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        if now >= self.deadline {
            return false;
        }
        tokio::time::sleep(self.next.min(self.deadline - now)).await;
        self.next = (self.next * 2).min(READINESS_MAX_POLL);
        true
    }

    /// What is left of the budget, for bounding an attempt rather than the gap
    /// between two of them.
    fn remaining(&self) -> std::time::Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }
}

/// Confirm a just-started container is running, findable by the same
/// workspace-label query `dev status`/`dev exec` use, *and* able to run a
/// command.
///
/// A successful create/start call is not proof of any of the three: issue #4
/// was a container that `dev up` had started but that neither command could
/// find, and whose create → start → wait sequence could not run a command even
/// once it was found. Reporting readiness is gated on this check so `dev up`
/// and the commands that follow it can never disagree.
async fn verify_container_usable(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    workspace: &Path,
    remote_user: Option<&str>,
) -> anyhow::Result<()> {
    verify_container_discoverable(runtime, container_id, workspace).await?;
    verify_container_execs(runtime, container_id, remote_user).await
}

/// Run one trivial command in the container, the way every later command does.
///
/// Discovery and a `Running` state only prove `dev exec` can *locate* the
/// container. A config with no lifecycle hooks and no dotfiles never execs
/// anything else during `dev up`, so without this a container whose exec path
/// is broken — the other half of issue #4 — would still be announced as ready
/// and only fail the first time the user asked it to do something.
///
/// What is being certified is the runtime's create → start → wait sequence, not
/// the image's contents. A reply of any exit status means that sequence works,
/// which is what `dev exec` needs; only a runtime that cannot run a process at
/// all fails the gate. So a scratch or distroless image with no shell is
/// reported and allowed through, while the hang and the lost exit status behind
/// issue #4 are not.
///
/// The probe runs as the resolved `remoteUser`, since that is who lifecycle
/// hooks, `dev exec` and `dev shell` run as — certifying root instead would
/// pass for a config whose user does not exist in the image.
///
/// The call itself is bounded by what remains of the readiness budget, not just
/// the gap between attempts. Issue #4's symptom was a `containerWait` the daemon
/// dropped, which leaves the exec awaiting an exit that never comes — so a gate
/// that only paced its retries would hang on the very failure it exists to
/// catch, with `dev up` silent after "Starting container...".
async fn verify_container_execs(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    remote_user: Option<&str>,
) -> anyhow::Result<()> {
    let probe = vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()];
    let mut polls = ReadinessPolls::new();

    loop {
        let attempt = tokio::time::timeout(
            polls.remaining(),
            runtime.exec(container_id, &probe, remote_user),
        )
        .await;

        let failure = match attempt {
            // A process that ran to completion proves the create → start → wait
            // sequence works, whatever status it reported — and that sequence
            // is the whole of what this gate certifies. The status belongs to
            // the image, so it is passed on as it actually is.
            Ok(Ok(result)) => {
                if result.exit_code != 0 {
                    warn_probe_exited_non_zero(container_id, remote_user, &result);
                }
                return Ok(());
            }
            Ok(Err(e)) if runtime.exec_reports_missing_command(&e) => {
                warn_probe_command_missing(container_id, &e.to_string());
                return Ok(());
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!(
                "the command was accepted but never reported an exit within {}s",
                READINESS_BUDGET.as_secs()
            ),
        };

        if !polls.wait().await {
            anyhow::bail!(
                "Container '{container_id}' was started and is running, but no command can be \
                 run in it: {failure}. `dev exec`, `dev shell` and lifecycle hooks would all \
                 fail the same way."
            );
        }
    }
}

/// Report a probe that ran and exited non-zero, saying what that status means.
///
/// The runtime is fine — it ran the process and reported its exit — so this is
/// a warning, not a readiness failure. What went wrong is worth getting right:
/// 127 and 126 are different problems, and calling a permission problem a
/// missing shell sends the user looking in the wrong place.
fn warn_probe_exited_non_zero(container_id: &str, remote_user: Option<&str>, result: &ExecResult) {
    let as_user = match remote_user {
        Some(user) => format!(" as user '{user}'"),
        None => String::new(),
    };
    let diagnosis = match result.exit_code {
        127 => "no `sh` was found in it".to_string(),
        126 => "`sh` is present but could not be executed — check its mode and whether this \
                user may run it"
            .to_string(),
        code => format!("`sh -c 'exit 0'` exited {code}"),
    };
    let reported = match result.stderr.trim() {
        "" => String::new(),
        stderr => format!(" The runtime reported: {stderr}"),
    };
    eprintln!(
        "Warning: container '{container_id}' runs commands{as_user}, but {diagnosis}. \
         Lifecycle hooks, `dev exec` and `dev shell` will fail the same way.{reported}"
    );
}

/// Report a runtime that declined to run the probe because the image has no
/// such command.
fn warn_probe_command_missing(container_id: &str, reason: &str) {
    eprintln!(
        "Warning: container '{container_id}' accepts commands but has no usable shell ({reason}). \
         Lifecycle hooks, `dev exec` and `dev shell` need one."
    );
}

/// Confirm a just-started container is actually running *and* findable by the
/// same workspace-label query `dev status`/`dev exec` use.
///
/// A failing list is retried like a missing one: this runs in the window where
/// a VM-backed runtime is still settling, so its list call is the most likely
/// thing to fail transiently, and aborting a healthy `dev up` on the first such
/// blip is exactly the spurious failure this gate must not introduce. The last
/// error is only surfaced once the budget is spent.
async fn verify_container_discoverable(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    workspace: &Path,
) -> anyhow::Result<()> {
    let filters: Vec<String> = workspace_labels(workspace, None)
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let mut polls = ReadinessPolls::new();
    // Assigned by every poll, so only the final one's outcome is reported.
    let mut last_error;
    let last_state = loop {
        let state = match runtime.list_containers(&filters).await {
            Ok(found) => {
                // A poll that answered clears the last failure: only a runtime
                // still failing at the deadline may claim the diagnosis, or an
                // early blip masks the not-discoverable report this gate exists
                // to produce.
                last_error = None;
                match found.iter().find(|c| same_container(&c.id, container_id)) {
                    Some(c) if c.state == ContainerState::Running => return Ok(()),
                    Some(c) => Some(c.state.clone()),
                    None => None,
                }
            }
            Err(e) => {
                last_error = Some(e);
                None
            }
        };

        if !polls.wait().await {
            break state;
        }
    };

    match (last_state, last_error) {
        (Some(state), _) => anyhow::bail!(
            "Container '{container_id}' was started but is {state:?}, not running. \
             Check the runtime's logs for why its init process exited."
        ),
        (None, Some(e)) => anyhow::bail!(
            "Container '{container_id}' was started but the runtime could not be asked \
             whether it is running: {e}"
        ),
        (None, None) => anyhow::bail!(
            "Container '{container_id}' was started but is not discoverable by the \
             workspace labels `dev status` and `dev exec` search for ({}). \
             The container exists but no command can reach it.",
            filters.join(", ")
        ),
    }
}

/// Compare a listed container id with the one create returned.
///
/// Runtimes are inconsistent about returning full or shortened ids, so a
/// prefix match on either side counts as the same container.
fn same_container(listed: &str, created: &str) -> bool {
    !listed.is_empty()
        && !created.is_empty()
        && (listed.starts_with(created) || created.starts_with(listed))
}

fn apply_cli_overrides(
    config: &mut DevcontainerConfig,
    port_overrides: &[String],
) -> anyhow::Result<()> {
    if !port_overrides.is_empty() {
        config.forward_ports = Some(parse_port_overrides(port_overrides)?);
    }
    Ok(())
}

/// Ensure a container image is present locally, pulling it only if missing.
///
/// Mirrors the reference devcontainer CLI behavior: inspect the local image
/// first and pull only when it is not already present. The progress message is
/// printed *before* the pull starts so the user is not left staring at a silent
/// prompt during a potentially long network pull.
pub(crate) async fn ensure_image_present(
    runtime: &dyn ContainerRuntime,
    image: &str,
) -> anyhow::Result<()> {
    if runtime.image_exists(image).await? {
        eprintln!("Using local image '{image}'...");
    } else {
        eprintln!("Pulling image '{image}'...");
        runtime.pull_image(image).await?;
    }
    Ok(())
}

/// Resolve the container capabilities contributed by features.
///
/// On the build path `ordered_features` is populated and is authoritative. On the
/// cache-hit path the features are never resolved, so recover the capabilities from the
/// `devcontainer.metadata` label the build wrote onto the image — otherwise a container
/// recreated from a cached image silently loses `privileged`, `capAdd`, `securityOpt`
/// and `init`, and a docker-in-docker daemon cannot start.
async fn resolve_container_capabilities(
    runtime: &dyn ContainerRuntime,
    image: &str,
    ordered_features: &[ResolvedFeature],
    has_features: bool,
) -> anyhow::Result<MergedCapabilities> {
    if !ordered_features.is_empty() {
        return Ok(merge_feature_capabilities(ordered_features));
    }
    if !has_features {
        return Ok(MergedCapabilities::default());
    }

    // Features were configured but not resolved, so this is the cache-hit path.
    let meta = runtime.inspect_image_metadata(image).await?;
    if meta.metadata_entries.is_empty() {
        eprintln!(
            "Warning: image '{image}' has no devcontainer metadata, so feature \
             capabilities (privileged, cap-add, security-opt) cannot be restored. \
             Run 'dev up --rebuild' to rebuild it."
        );
        return Ok(MergedCapabilities::default());
    }

    Ok(capabilities_from_metadata(&meta.metadata_entries))
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
            for command in commands.values() {
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
    let target = dotfiles.target_path.as_deref().unwrap_or("~/dotfiles");

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
        let args = vec!["sh".to_string(), "-c".to_string(), install_cmd.clone()];
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
    update_remote_user_uid_default: &str,
    lockfile: &LockfilePolicy,
) -> anyhow::Result<()> {
    let compose_data = config.docker_compose_file.as_ref().unwrap();
    let compose_files = compose_data.files();
    let devcontainer_dir = config_path.parent().unwrap();
    let devcontainer_dir_buf: Option<PathBuf> = Some(devcontainer_dir.to_path_buf());
    let service = config
        .service
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Docker Compose config must specify 'service'"))?;
    let project_name = container_name(workspace);
    let folder_image = container_name(workspace);
    let runtime_name = runtime.runtime_name();

    // Workspace-related env vars for Docker Compose variable interpolation.
    // Compose files use ${localWorkspaceFolder}, ${localWorkspaceFolderBasename},
    // etc. in volume paths and other settings. These must be set as process env
    // vars so `docker compose` resolves them when parsing the compose file.
    let folder_name = workspace_folder_name(workspace);
    let workspace_source = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let workspace_target = substitute_variables(
        config
            .workspace_folder
            .as_deref()
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
        runtime_name,
        &compose_files,
        devcontainer_dir,
        Some(service),
        no_cache,
        verbose,
        &compose_env,
    )
    .await?;

    // 3. Get the built service image name.
    let base_image = crate::runtime::compose::compose_service_image(
        runtime_name,
        &compose_files,
        devcontainer_dir,
        &project_name,
        service,
        &compose_env,
    )
    .await?;
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
        lockfile.apply(devcontainer_dir_buf.as_deref(), &features)?;

        let ordered = order_features(&features);
        if verbose {
            eprintln!("Feature install order:");
            for (i, f) in ordered.iter().enumerate() {
                eprintln!(
                    "  {}: {}{}",
                    i + 1,
                    f.id,
                    if f.is_dependency { " (dependency)" } else { "" }
                );
            }
        }

        let staging_dir = stage_feature_context(&ordered)?;
        let feature_user =
            resolve_remote_user(runtime, &base_image, config.remote_user.as_deref()).await?;
        let feature_tag = feature_image_tag(&folder_image, config, &ordered);
        let dockerfile = generate_feature_dockerfile_with_opts(
            &base_image,
            &ordered,
            feature_user.as_deref(),
            config,
        );
        if verbose {
            eprintln!("Features Dockerfile:\n{dockerfile}");
        }
        eprintln!("Building features image...");
        let result = runtime
            .build_image(
                &dockerfile,
                &staging_dir,
                &feature_tag,
                &HashMap::new(),
                no_cache,
                verbose,
            )
            .await;
        let _ = std::fs::remove_dir_all(&staging_dir);
        result.map_err(|e| anyhow::anyhow!("{e}"))?;

        ordered_features = ordered;
        feature_tag
    } else {
        base_image.clone()
    };

    // 5. Resolve remote user from the final image.
    let effective_user =
        resolve_remote_user(runtime, &featured_image, config.remote_user.as_deref()).await?;
    let remote_user = effective_user.as_deref();

    // 6. UID remapping.
    let final_image = if uid::should_remap_uid(config, remote_user, update_remote_user_uid_default)
    {
        let image_meta = runtime
            .inspect_image_metadata(&featured_image)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let image_user = image_meta.container_user.as_deref().unwrap_or("root");
        uid::build_uid_image(
            runtime,
            &featured_image,
            &folder_image,
            remote_user.unwrap_or("root"),
            image_user,
            no_cache,
            verbose,
        )
        .await?
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
            env.insert(
                k.clone(),
                substitute_variables_with_user(v, workspace, remote_user),
            );
        }
    }
    if let Some(ref remote_env) = config.remote_env {
        for (k, v) in remote_env {
            env.insert(
                k.clone(),
                substitute_variables_with_user(v, workspace, remote_user),
            );
        }
    }

    let mounts = substitute_mounts(
        config.mounts.as_deref().unwrap_or(&[]),
        workspace,
        remote_user,
    );

    let volume_strings: Vec<String> = config
        .volumes
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| substitute_variables_with_user(s, workspace, remote_user))
        .collect();

    let ports: Vec<PortMapping> = config.forward_ports.clone().unwrap_or_default();
    let caddy_host_ports_compose: Vec<crate::caddy::PortEntry> = ports
        .iter()
        .map(|p| crate::caddy::PortEntry {
            port: p.host,
            custom_name: None,
            keepalive: None,
        })
        .collect();

    // 8. Labels + merged feature capabilities.
    let labels_list = workspace_labels(workspace, Some(config_path));
    let caps = merge_feature_capabilities(&ordered_features);

    // 9. Generate and write compose override file.
    let override_content = crate::runtime::compose::generate_compose_override(
        service,
        &labels_list,
        &env,
        &mounts,
        &volume_strings,
        &ports,
        image_override,
        &caps,
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
        runtime_name,
        &up_file_refs,
        devcontainer_dir,
        &project_name,
        &compose_env,
        verbose,
    )
    .await?;

    // 11. Get container ID.
    let container_id = crate::runtime::compose::compose_container_id(
        runtime_name,
        &up_file_refs,
        devcontainer_dir,
        &project_name,
        service,
    )
    .await?;

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

    println!(
        "Compose service '{service}' is ready (container {}).",
        &container_id[..12.min(container_id.len())]
    );

    if !caddy_host_ports_compose.is_empty()
        && let Err(e) = crate::caddy::register_site(workspace, &caddy_host_ports_compose)
    {
        eprintln!("Warning: Caddy setup failed: {e}");
    }

    Ok(())
}

/// Substitute variables in each mount entry (string or object form) and emit
/// Docker long-form strings, warning about entries that lack `source`/`target`.
fn substitute_mounts(
    mounts: &[MountSpec],
    workspace: &Path,
    remote_user: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    for m in mounts {
        if let Some(emitted) = m.substitute_and_emit(workspace, remote_user) {
            out.push(emitted);
        } else {
            eprintln!("Warning: mount entry is missing source or target; skipping: {m:?}");
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::{
        apply_cli_overrides, ensure_image_present, parse_mounts, parse_single_mount,
        substitute_mounts,
    };
    use crate::devcontainer::config::{DevcontainerConfig, MountObject, MountSpec};
    use crate::devcontainer::effective::load_effective_config_value;
    use crate::error::DevError;
    use crate::runtime::{
        AttachedExec, BoxFut, ContainerConfig, ContainerInfo, ContainerRuntime, ContainerState,
        ExecResult, ImageMetadata,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn unused<T>() -> BoxFut<'static, T> {
        Box::pin(async {
            Err(DevError::Runtime(
                "FakeRuntime method unused by ensure_image_present".into(),
            ))
        })
    }

    fn write_project_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let devcontainer_dir = dir.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let path = devcontainer_dir.join("devcontainer.json");
        fs::write(&path, content).unwrap();
        path
    }

    fn write_base_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let base_dir = dir.path().join("base");
        fs::create_dir_all(&base_dir).unwrap();
        let path = base_dir.join("devcontainer.json");
        fs::write(&path, content).unwrap();
        path
    }

    fn load_config_with_base(
        config_path: &Path,
        include_base: bool,
        base_config_path: &Path,
    ) -> DevcontainerConfig {
        let (value, _) = load_effective_config_value(config_path, include_base, base_config_path)
            .expect("effective config should load");
        serde_json::from_value(value).expect("effective config should deserialize")
    }

    #[test]
    fn cli_port_overrides_apply_last() {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let config_path = write_project_config(
            &workspace,
            r#"{"image": "ubuntu:24.04", "forwardPorts": [3000]}"#,
        );
        let base_path = write_base_config(&home, r#"{"forwardPorts": [8080]}"#);
        let mut config = load_config_with_base(&config_path, true, &base_path);

        apply_cli_overrides(&mut config, &["9090:90".to_string(), "7070".to_string()]).unwrap();

        let ports = config.forward_ports.unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].host, 9090);
        assert_eq!(ports[0].container, 90);
        assert_eq!(ports[1].host, 7070);
        assert_eq!(ports[1].container, 7070);
    }

    /// Minimal fake runtime: records `pull_image` calls and returns a fixed
    /// `image_exists` result. Every other trait method is unused by
    /// `ensure_image_present` and returns an error if invoked.
    struct FakeRuntime {
        exists: AtomicBool,
        pull_count: AtomicUsize,
    }

    impl FakeRuntime {
        fn new(exists: bool) -> Self {
            Self {
                exists: AtomicBool::new(exists),
                pull_count: AtomicUsize::new(0),
            }
        }

        fn pull_count(&self) -> usize {
            self.pull_count.load(Ordering::SeqCst)
        }
    }

    impl ContainerRuntime for FakeRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake"
        }

        fn pull_image(&self, _image: &str) -> BoxFut<'_, ()> {
            self.pull_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(()) })
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

        fn exec(&self, _id: &str, _cmd: &[String], _user: Option<&str>) -> BoxFut<'_, ExecResult> {
            unused()
        }

        fn exec_interactive(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'_, i32> {
            unused()
        }

        fn inspect_container(&self, _id: &str) -> BoxFut<'_, ContainerInfo> {
            unused()
        }

        fn list_containers(&self, _label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
            unused()
        }

        fn image_exists(&self, _image: &str) -> BoxFut<'_, bool> {
            let exists = self.exists.load(Ordering::SeqCst);
            Box::pin(async move { Ok(exists) })
        }

        fn inspect_image_metadata(&self, _image: &str) -> BoxFut<'_, ImageMetadata> {
            unused()
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

    /// When the image is already present locally, `ensure_image_present` must
    /// use it and must NOT pull.
    #[tokio::test]
    async fn ensure_image_present_skips_pull_when_image_exists() {
        let rt = FakeRuntime::new(true);
        ensure_image_present(&rt, "localimg:latest")
            .await
            .expect("helper should succeed when image exists");
        assert_eq!(
            rt.pull_count(),
            0,
            "pull_image must not be called when image_exists returns true"
        );
    }

    /// When the image is missing locally, `ensure_image_present` must pull it
    /// exactly once.
    #[tokio::test]
    async fn ensure_image_present_pulls_when_image_missing() {
        let rt = FakeRuntime::new(false);
        ensure_image_present(&rt, "remoteimg:latest")
            .await
            .expect("helper should succeed after pulling");
        assert_eq!(
            rt.pull_count(),
            1,
            "pull_image must be called exactly once when image_exists returns false"
        );
    }

    /// Regression test for issue #24: the build/features base-image path routes
    /// through `ensure_image_present` and therefore skips the pull when the
    /// image is already local. Mirrors the image-only branch's behavior.
    #[tokio::test]
    async fn build_path_base_image_skips_pull_when_image_exists() {
        let rt = FakeRuntime::new(true);
        // Base-image determination in the build/features branch:
        let image_name = "localimg:latest";
        ensure_image_present(&rt, image_name)
            .await
            .expect("helper should succeed when image exists locally");
        assert_eq!(
            rt.pull_count(),
            0,
            "build path must not call pull_image when image_exists returns true"
        );
    }

    /// `parse_single_mount` must accept a bind-mount long-form string.
    #[test]
    fn parse_single_mount_accepts_bind_long_form() {
        let m = parse_single_mount("source=./,target=/workspace,type=bind,readonly=true")
            .expect("long-form bind mount should parse");
        assert_eq!(m.source, std::path::PathBuf::from("./"));
        assert_eq!(m.target, "/workspace");
        assert!(m.readonly);
    }

    /// `parse_single_mount` must accept a bind-mount long-form string with `ro` flag.
    #[test]
    fn parse_single_mount_accepts_long_form_with_ro() {
        let m = parse_single_mount("source=/host,target=/container,readonly,ro")
            .expect("long-form bind mount with ro keyword should parse");
        assert!(m.readonly);
    }

    /// `parse_single_mount` accepts a non-bind long-form string (type is
    /// ignored; Docker treats a bare source name as a named volume).
    #[test]
    fn parse_single_mount_accepts_non_bind_type() {
        let m = parse_single_mount("source=myvol,target=/data,type=volume")
            .expect("non-bind mount should still parse (type is ignored)");
        assert_eq!(m.source, std::path::PathBuf::from("myvol"));
        assert_eq!(m.target, "/data");
        assert!(!m.readonly);
    }

    /// Non-bind mounts must NOT be dropped: a `type=volume` mount, in either
    /// string or object form, is rendered as a `BindMount` through the same
    /// `substitute_mounts` + `parse_mounts` chain `run` uses.
    #[test]
    fn volume_type_mount_is_rendered_not_dropped() {
        let ws = std::path::Path::new("/home/user/project");
        let specs = vec![
            MountSpec::Plain("source=myvol,target=/data,type=volume".to_string()),
            MountSpec::Object(MountObject {
                source: Some("othervol".to_string()),
                target: Some("/cache".to_string()),
                r#type: Some("volume".to_string()),
                ..Default::default()
            }),
        ];
        let strings = substitute_mounts(&specs, ws, None);
        let mounts = parse_mounts(&strings);
        assert_eq!(mounts.len(), 2, "volume-type mounts must not be dropped");
        assert_eq!(mounts[0].source, std::path::PathBuf::from("myvol"));
        assert_eq!(mounts[0].target, "/data");
        assert!(!mounts[0].readonly);
        assert_eq!(mounts[1].source, std::path::PathBuf::from("othervol"));
        assert_eq!(mounts[1].target, "/cache");
    }

    /// An object mount missing `source` is skipped (with a warning) rather
    /// than rendered, while valid entries in the same list survive.
    #[test]
    fn malformed_object_mount_is_skipped_valid_ones_survive() {
        let ws = std::path::Path::new("/home/user/project");
        let specs = vec![
            MountSpec::Object(MountObject {
                source: None,
                target: Some("/data".to_string()),
                ..Default::default()
            }),
            MountSpec::Plain("/host:/container".to_string()),
        ];
        let strings = substitute_mounts(&specs, ws, None);
        let mounts = parse_mounts(&strings);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target, "/container");
    }

    // ---- issue #4 regression coverage: create/start error propagation and
    // workspace-label discovery ----
    //
    // `run_with_runtime` is the seam `run` delegates to after runtime
    // detection. These tests drive it with a stand-in runtime over a minimal
    // image-based devcontainer.json so the create/start/readiness flow can be
    // exercised deterministically in CI (no container daemon).

    use crate::util::workspace_labels;
    use std::sync::{Arc, Mutex};

    /// One command `exec` was asked to run, and the user it ran as.
    type ExecCall = (Vec<String>, Option<String>);

    /// Stand-in runtime for `run_with_runtime`, modelling a daemon: created
    /// containers land in `containers`, `start_container` marks them running,
    /// and `list_containers` answers label queries out of that same state — so
    /// the create → discover contract is exercised end to end.
    ///
    /// Each knob reproduces one way the issue #4 path failed: create/start
    /// erroring, the container never reaching running, and the container
    /// existing but being invisible to the label query `dev status`/`dev exec`
    /// use (which is what an undecodable `containerList` reply looks like).
    struct UpFakeRuntime {
        image_exists: bool,
        create_fails: bool,
        start_fails: bool,
        discoverable: bool,
        starts_running: bool,
        /// Number of list calls that still report the container as stopped,
        /// standing in for daemon-side state that settles late.
        running_after_polls: Arc<AtomicUsize>,
        /// Number of list calls that fail before the runtime answers at all,
        /// standing in for a daemon whose list/XPC call is still settling.
        list_errors_before_success: Arc<AtomicUsize>,
        /// Every list call fails, standing in for a runtime that never answers.
        list_always_fails: bool,
        /// `exec` errors, standing in for a container whose create → start →
        /// wait sequence never reports a command's exit.
        exec_fails: bool,
        /// Number of exec calls that fail before the runtime answers at all,
        /// standing in for an exec endpoint that is still coming up.
        exec_errors_before_success: Arc<AtomicUsize>,
        /// `exec` fails the way a runtime reports an image without the
        /// requested executable, rather than a broken exec path.
        exec_command_missing: bool,
        /// `exec` never returns, standing in for the dropped `containerWait`
        /// behind issue #4: the process runs but its exit is never reported.
        exec_never_returns: bool,
        /// `exec` answers, but with a non-zero status for a command that
        /// cannot fail on its own.
        exec_exit_code: i32,
        /// Commands `exec` was asked to run, with the user each ran as, so the
        /// gate's probe is observable.
        execs: Arc<Mutex<Vec<ExecCall>>>,
        created_id: String,
        created_config: Arc<Mutex<Option<ContainerConfig>>>,
        started_id: Arc<Mutex<Option<String>>>,
        containers: Arc<Mutex<Vec<ContainerInfo>>>,
    }

    impl UpFakeRuntime {
        fn ok() -> Self {
            Self {
                image_exists: true,
                create_fails: false,
                start_fails: false,
                discoverable: true,
                starts_running: true,
                running_after_polls: Arc::new(AtomicUsize::new(0)),
                list_errors_before_success: Arc::new(AtomicUsize::new(0)),
                list_always_fails: false,
                exec_fails: false,
                exec_errors_before_success: Arc::new(AtomicUsize::new(0)),
                exec_command_missing: false,
                exec_never_returns: false,
                exec_exit_code: 0,
                execs: Arc::new(Mutex::new(Vec::new())),
                created_id: "fake-id".to_string(),
                created_config: Arc::new(Mutex::new(None)),
                started_id: Arc::new(Mutex::new(None)),
                containers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Create, start and discovery all succeed, but no command can be run
        /// in the container.
        fn cannot_exec() -> Self {
            Self {
                exec_fails: true,
                ..Self::ok()
            }
        }

        /// Create, start and discovery all succeed, but a command that cannot
        /// fail on its own comes back non-zero — an image with no shell.
        fn execs_report(exit_code: i32) -> Self {
            Self {
                exec_exit_code: exit_code,
                ..Self::ok()
            }
        }

        /// The exec path works, but the runtime reports the image has no such
        /// executable, the way docker declines to start one.
        fn has_no_shell() -> Self {
            Self {
                exec_fails: true,
                exec_command_missing: true,
                ..Self::ok()
            }
        }

        /// The exec endpoint refuses the first `calls` attempts and then works,
        /// standing in for a runtime that is still settling.
        fn execs_after(calls: usize) -> Self {
            Self {
                exec_errors_before_success: Arc::new(AtomicUsize::new(calls)),
                ..Self::ok()
            }
        }

        /// The command is accepted but its exit is never reported — the
        /// dropped `containerWait` behind issue #4.
        fn execs_never_return() -> Self {
            Self {
                exec_never_returns: true,
                ..Self::ok()
            }
        }

        fn execs(&self) -> Vec<ExecCall> {
            self.execs.lock().unwrap().clone()
        }

        fn failing(create: bool, start: bool) -> Self {
            Self {
                create_fails: create,
                start_fails: start,
                ..Self::ok()
            }
        }

        /// Create and start succeed, but the container never shows up in the
        /// workspace-label query.
        fn undiscoverable() -> Self {
            Self {
                discoverable: false,
                ..Self::ok()
            }
        }

        /// Create and start succeed, but the container never reaches running.
        fn never_running() -> Self {
            Self {
                starts_running: false,
                ..Self::ok()
            }
        }

        /// Create and start succeed, and the container reports running only
        /// after `polls` list calls.
        fn running_late(polls: usize) -> Self {
            Self {
                running_after_polls: Arc::new(AtomicUsize::new(polls)),
                ..Self::ok()
            }
        }

        /// Create and start succeed, but the first `polls` list calls fail
        /// before the runtime answers at all.
        fn listing_fails_at_first(polls: usize) -> Self {
            Self {
                list_errors_before_success: Arc::new(AtomicUsize::new(polls)),
                ..Self::ok()
            }
        }

        /// Create and start succeed, but the runtime can never be listed.
        fn listing_never_works() -> Self {
            Self {
                list_always_fails: true,
                ..Self::ok()
            }
        }

        /// The first `polls` list calls fail, and every later one succeeds
        /// while still not showing the container.
        fn undiscoverable_after_a_blip(polls: usize) -> Self {
            Self {
                list_errors_before_success: Arc::new(AtomicUsize::new(polls)),
                ..Self::undiscoverable()
            }
        }

        fn created_config(&self) -> ContainerConfig {
            self.created_config
                .lock()
                .unwrap()
                .clone()
                .expect("create_container was not called")
        }
    }

    impl ContainerRuntime for UpFakeRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake"
        }

        fn pull_image(&self, _image: &str) -> BoxFut<'_, ()> {
            // image_exists returns true, so pull_image must never be reached.
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

        fn create_container(&self, config: &ContainerConfig) -> BoxFut<'_, String> {
            let config = config.clone();
            let create_fails = self.create_fails;
            let created_id = self.created_id.clone();
            let capture = self.created_config.clone();
            let containers = self.containers.clone();
            Box::pin(async move {
                if create_fails {
                    return Err(DevError::Runtime(
                        "create_container failed (test-injected)".to_string(),
                    ));
                }
                containers.lock().unwrap().push(ContainerInfo {
                    id: created_id.clone(),
                    name: config.name.clone(),
                    state: ContainerState::Stopped,
                    labels: config.labels.clone(),
                    image: config.image.clone(),
                });
                *capture.lock().unwrap() = Some(config);
                Ok(created_id)
            })
        }

        fn start_container(&self, id: &str) -> BoxFut<'_, ()> {
            let id = id.to_string();
            let start_fails = self.start_fails;
            let starts_running = self.starts_running;
            let capture = self.started_id.clone();
            let containers = self.containers.clone();
            Box::pin(async move {
                if start_fails {
                    return Err(DevError::Runtime(
                        "start_container failed (test-injected)".to_string(),
                    ));
                }
                if starts_running {
                    for container in containers.lock().unwrap().iter_mut() {
                        if container.id == id {
                            container.state = ContainerState::Running;
                        }
                    }
                }
                *capture.lock().unwrap() = Some(id);
                Ok(())
            })
        }

        fn stop_container(&self, _id: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn remove_container(&self, _id: &str) -> BoxFut<'_, ()> {
            unused()
        }

        fn exec(&self, _id: &str, cmd: &[String], user: Option<&str>) -> BoxFut<'_, ExecResult> {
            let cmd = cmd.to_vec();
            let user = user.map(str::to_string);
            let exec_fails = self.exec_fails;
            let command_missing = self.exec_command_missing;
            let exit_code = self.exec_exit_code;
            let settling = self.exec_errors_before_success.clone();
            let never_returns = self.exec_never_returns;
            let execs = self.execs.clone();
            Box::pin(async move {
                execs.lock().unwrap().push((cmd, user));
                if never_returns {
                    std::future::pending::<()>().await;
                }
                let still_settling = settling
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_ok();
                if exec_fails || still_settling {
                    return Err(DevError::Runtime(if command_missing {
                        "OCI runtime exec failed: exec: \"sh\": executable file not found in $PATH"
                            .to_string()
                    } else {
                        "exec failed (test-injected)".to_string()
                    }));
                }
                Ok(ExecResult {
                    exit_code,
                    stdout: String::new(),
                    stderr: if exit_code == 0 {
                        String::new()
                    } else {
                        "sh: not found".to_string()
                    },
                })
            })
        }

        /// Classified the way the docker runtime classifies it, so the gate's
        /// tolerance is exercised through the same seam production uses.
        fn exec_reports_missing_command(&self, error: &DevError) -> bool {
            error.to_string().contains("executable file not found")
        }

        fn exec_interactive(
            &self,
            _id: &str,
            _cmd: &[String],
            _user: Option<&str>,
        ) -> BoxFut<'_, i32> {
            unused()
        }

        fn inspect_container(&self, _id: &str) -> BoxFut<'_, ContainerInfo> {
            unused()
        }

        fn list_containers(&self, label_filters: &[String]) -> BoxFut<'_, Vec<ContainerInfo>> {
            let filters: Vec<(String, String)> = label_filters
                .iter()
                .map(|f| {
                    let (key, value) = f.split_once('=').unwrap_or((f.as_str(), ""));
                    (key.to_string(), value.to_string())
                })
                .collect();
            let discoverable = self.discoverable;
            let containers = self.containers.clone();
            let settling = self.running_after_polls.clone();
            let list_errors = self.list_errors_before_success.clone();
            let list_always_fails = self.list_always_fails;
            let started = self.started_id.clone();
            Box::pin(async move {
                // The injected list failures model the window right after
                // start, which is the only one the readiness gate polls in.
                if started.lock().unwrap().is_some() {
                    let transient = list_errors
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                        .is_ok();
                    if transient || list_always_fails {
                        return Err(DevError::Runtime(
                            "list_containers failed (test-injected)".to_string(),
                        ));
                    }
                }
                if !discoverable {
                    return Ok(Vec::new());
                }
                let still_settling = settling
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_ok();
                let known = containers.lock().unwrap().clone();
                Ok(known
                    .into_iter()
                    .filter(|c| {
                        filters
                            .iter()
                            .all(|(key, value)| c.labels.get(key).is_some_and(|got| got == value))
                    })
                    .map(|mut c| {
                        if still_settling {
                            c.state = ContainerState::Stopped;
                        }
                        c
                    })
                    .collect())
            })
        }

        fn image_exists(&self, _image: &str) -> BoxFut<'_, bool> {
            let exists = self.image_exists;
            Box::pin(async move { Ok(exists) })
        }

        fn inspect_image_metadata(&self, _image: &str) -> BoxFut<'_, ImageMetadata> {
            // No remote user, and update_remote_user_uid_default="never" skips
            // UID remap, so the up flow never advances past create/start here.
            Box::pin(async move { Ok(ImageMetadata::default()) })
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

    /// Drive `run_with_runtime` over a minimal image-based workspace.
    async fn run_up_with_fake(rt: &UpFakeRuntime, workspace: &TempDir) -> anyhow::Result<()> {
        super::run_with_runtime(
            workspace.path(),
            rt,
            /* rebuild */ false,
            /* no_cache */ false,
            /* verbose */ false,
            /* frozen_lockfile */ false,
            /* update_remote_user_uid_default */ "never",
            /* port_overrides */ &[],
            /* no_base */ true,
        )
        .await
    }

    /// A failed `create_container` must propagate as an error from `dev up` —
    /// no readiness may be reported when the container was not created. This
    /// is the core of issue #4's "must not report readiness unless actually
    /// created" acceptance.
    #[tokio::test]
    async fn up_propagates_create_container_error() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::failing(true, false);
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("create_container failure must surface as an error");
        let msg = format!("{err}");
        assert!(
            msg.contains("create_container failed"),
            "error should mention create failure, got: {msg}"
        );
    }

    /// A failed `start_container` must propagate as an error from `dev up` —
    /// readiness must not survive a start failure (issue #4).
    #[tokio::test]
    async fn up_propagates_start_container_error() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::failing(false, true);
        run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("start_container failure must surface as an error");
    }

    /// The container config `dev up` hands to the runtime must carry the
    /// `devcontainer.local_folder` workspace label, and that label must match
    /// the one `dev status`/`dev exec` query with (`workspace_labels(workspace, None)`).
    ///
    /// On the issue #4 broken path, discovery was ID-based and the workspace
    /// label was not the join key, so `dev status`/`dev exec` could not find a
    /// container that `dev up` had just created. This pins the creation to
    /// discovery contract at the `up` layer; the Apple-runtime half is pinned
    /// in `runtime::apple::tests::to_apple_config_truncates_id_and_carries_discovery_label`.
    #[tokio::test]
    async fn up_labels_container_with_workspace_local_folder() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::ok();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("up should succeed with a cooperating fake runtime");

        let created = rt.created_config();
        let discovery_labels = workspace_labels(workspace.path(), None);
        for (key, value) in &discovery_labels {
            assert_eq!(
                created.labels.get(key),
                Some(value),
                "dev up must set the {key} label used by dev status/dev exec"
            );
        }
        let local_folder = created
            .labels
            .get("devcontainer.local_folder")
            .expect("devcontainer.local_folder label must be set");
        let abs_workspace = workspace
            .path()
            .canonicalize()
            .unwrap_or_else(|_| workspace.path().to_path_buf());
        assert_eq!(
            local_folder,
            &abs_workspace.to_string_lossy().to_string(),
            "local_folder label must be the absolute workspace path"
        );
    }

    /// The reported issue #4 failure: create and start both succeed, but the
    /// container cannot be found by the workspace labels `dev status`/`dev exec`
    /// query with. `dev up` must fail instead of announcing readiness.
    #[tokio::test(start_paused = true)]
    async fn up_fails_when_started_container_is_not_discoverable() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::undiscoverable();
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("readiness must not be reported for an undiscoverable container");
        let msg = format!("{err}");
        assert!(
            msg.contains("not discoverable"),
            "error should explain the discovery failure, got: {msg}"
        );
    }

    /// A container that is created and started but never reaches the running
    /// state is not usable by `dev exec`, so `dev up` must not report readiness.
    #[tokio::test(start_paused = true)]
    async fn up_fails_when_started_container_never_runs() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::never_running();
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("readiness must not be reported for a container that is not running");
        let msg = format!("{err}");
        assert!(
            msg.contains("not running"),
            "error should explain the container is not running, got: {msg}"
        );
    }

    /// A monorepo config: `workspaceMount` attaches the repository, and
    /// `workspaceFolder` selects one project inside it. `dev up` must hand the
    /// runtime both — the mount root for the bind, the subdirectory for where
    /// commands run — or lifecycle hooks execute in the wrong directory.
    #[tokio::test]
    async fn up_carries_workspace_folder_subdirectory_to_the_runtime() {
        let workspace = TempDir::new().unwrap();
        write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "workspaceMount": "source=${localWorkspaceFolder},target=/srv/app,type=bind",
                "workspaceFolder": "/srv/app/packages/api"
            }"#,
        );
        let rt = UpFakeRuntime::ok();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("up should succeed with a cooperating fake runtime");

        let created = rt.created_config();
        assert_eq!(
            created.workspace_mount.as_ref().map(|m| m.target.as_str()),
            Some("/srv/app"),
            "the repository must still be mounted at the workspaceMount target"
        );
        assert_eq!(
            created.workspace_folder.as_deref(),
            Some("/srv/app/packages/api"),
            "commands must run in the configured workspaceFolder subdirectory"
        );
    }

    /// Without an explicit `workspaceFolder`, the folder handed to the runtime
    /// is the mount destination, so commands still run in the source tree.
    #[tokio::test]
    async fn up_defaults_workspace_folder_to_the_mount_target() {
        let workspace = TempDir::new().unwrap();
        write_project_config(
            &workspace,
            r#"{
                "image": "ubuntu:24.04",
                "workspaceMount": "source=${localWorkspaceFolder},target=/srv/app,type=bind"
            }"#,
        );
        let rt = UpFakeRuntime::ok();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("up should succeed with a cooperating fake runtime");

        let created = rt.created_config();
        assert_eq!(created.workspace_folder.as_deref(), Some("/srv/app"));
    }

    /// A healthy container whose runtime takes a while to report `Running` must
    /// be waited for, not failed. Twelve polls is past what a fixed
    /// hundred-millisecond-per-attempt budget would tolerate, which is the
    /// spurious failure a VM-backed runtime would hit.
    #[tokio::test(start_paused = true)]
    async fn up_waits_for_a_container_that_reports_running_late() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::running_late(12);
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("a container that settles late must still be accepted");
    }

    /// The readiness gate polls in exactly the window where a VM-backed
    /// runtime's list/XPC call is most likely to fail transiently. A blip
    /// there must be retried like a not-yet-visible container, not turned into
    /// a hard failure for a container that is coming up fine.
    #[tokio::test(start_paused = true)]
    async fn up_retries_a_transient_list_failure_while_waiting_for_readiness() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::listing_fails_at_first(5);
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("a transient list failure must not abort a healthy `dev up`");
    }

    /// Tolerating list failures must not become ignoring them: a runtime that
    /// never answers has to fail the gate with the reason, not with a
    /// misleading "not discoverable".
    #[tokio::test(start_paused = true)]
    async fn up_reports_the_list_failure_when_it_never_clears() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::listing_never_works();
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("readiness must not be reported when the runtime cannot be asked");
        let msg = format!("{err}");
        assert!(
            msg.contains("list_containers failed"),
            "the error must carry the runtime's own failure, got: {msg}"
        );
    }

    /// A blip that recovered must not claim the diagnosis. Once a later poll
    /// answers, the container really is absent from the workspace-label query
    /// — the exact issue #4 symptom — and that is what has to be reported,
    /// not a transient error the runtime already recovered from.
    #[tokio::test(start_paused = true)]
    async fn up_reports_undiscoverable_when_an_early_list_failure_recovered() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::undiscoverable_after_a_blip(1);
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("readiness must not be reported for an undiscoverable container");
        let msg = format!("{err}");
        assert!(
            msg.contains("not discoverable"),
            "a recovered blip must not mask the discovery diagnosis, got: {msg}"
        );
        assert!(
            !msg.contains("list_containers failed"),
            "the stale error must not be reported once a later poll answered, got: {msg}"
        );
    }

    /// The acceptance criterion is that `dev up` must not report readiness for
    /// a container `dev exec` cannot use — and discovery plus `Running` only
    /// proves `dev exec` can *find* it. A config with no lifecycle hooks and no
    /// dotfiles never execs anything else, so the gate has to run a command
    /// itself or a broken exec path is announced as ready.
    #[tokio::test]
    async fn up_runs_a_command_in_the_container_before_reporting_readiness() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::ok();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("up should succeed with a cooperating fake runtime");

        assert!(
            !rt.execs().is_empty(),
            "readiness must be gated on a command actually running in the container"
        );
    }

    /// The gate certifies the user every later command runs as. Probing as root
    /// would pass for a config whose `remoteUser` does not exist in the image,
    /// and the first `dev exec` would then fail on user resolution.
    #[tokio::test]
    async fn the_readiness_probe_runs_as_the_resolved_remote_user() {
        let workspace = TempDir::new().unwrap();
        write_project_config(
            &workspace,
            r#"{"image":"ubuntu:24.04","remoteUser":"vscode"}"#,
        );
        let rt = UpFakeRuntime::ok();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("up should succeed with a cooperating fake runtime");

        let (_, user) = rt.execs().first().cloned().expect("the gate must probe");
        assert_eq!(
            user.as_deref(),
            Some("vscode"),
            "the probe must run as the user lifecycle hooks and `dev exec` use"
        );
    }

    /// The exec endpoint of a VM-backed runtime can still be coming up when the
    /// daemon already reports `Running`. The discovery half waits that out, so
    /// the probe must too — a one-shot check would fail a healthy container.
    #[tokio::test(start_paused = true)]
    async fn up_retries_a_transient_exec_failure_while_waiting_for_readiness() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::execs_after(5);
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("a settling exec endpoint must not abort a healthy `dev up`");
    }

    /// Issue #4's actual symptom: the daemon drops the exit wait, so the exec
    /// never returns at all. Pacing the gaps between failures does not catch
    /// that — nothing ever fails — so the gate has to bound the call itself or
    /// `dev up` hangs silently after "Starting container..." forever.
    #[tokio::test(start_paused = true)]
    async fn up_fails_when_the_probe_never_reports_an_exit() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::execs_never_return();

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            run_up_with_fake(&rt, &workspace),
        )
        .await
        .expect("the readiness gate must bound the probe rather than wait forever")
        .expect_err("readiness must not be reported for a container whose exec never returns");

        let msg = format!("{err}");
        assert!(
            msg.contains("never reported an exit"),
            "the error must name the hang, got: {msg}"
        );
    }

    /// The other half of issue #4: the container is created, started and
    /// discoverable, but its exec path never reports a command's exit.
    #[tokio::test(start_paused = true)]
    async fn up_fails_when_no_command_can_run_in_the_started_container() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
        let rt = UpFakeRuntime::cannot_exec();
        let err = run_up_with_fake(&rt, &workspace)
            .await
            .expect_err("readiness must not be reported for a container that cannot exec");
        let msg = format!("{err}");
        assert!(
            msg.contains("no command can be run in it"),
            "error should explain the exec failure, got: {msg}"
        );
    }

    /// What the gate certifies is the runtime's create → start → wait sequence,
    /// not the image's contents. A shell-less scratch or distroless image
    /// answers the probe with a non-zero status — which means that sequence
    /// worked — so `dev up` reports the missing shell and still comes up.
    #[tokio::test(start_paused = true)]
    async fn up_tolerates_an_image_whose_shell_is_missing() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"scratch"}"#);
        let rt = UpFakeRuntime::execs_report(127);
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("an image without a shell must not fail a container that runs commands");
    }

    /// Any status a completed process reported is proof the transport worked,
    /// so it is accepted without a second attempt — the probe only retries a
    /// runtime that could not run the process at all.
    #[tokio::test(start_paused = true)]
    async fn up_accepts_any_status_a_completed_probe_reported() {
        for exit_code in [126, 127, 1] {
            let workspace = TempDir::new().unwrap();
            write_project_config(&workspace, r#"{"image":"ubuntu:24.04"}"#);
            let rt = UpFakeRuntime::execs_report(exit_code);
            run_up_with_fake(&rt, &workspace)
                .await
                .unwrap_or_else(|e| panic!("exit {exit_code} proves the exec path works: {e}"));
            assert_eq!(
                rt.execs().len(),
                1,
                "a process that ran needs no retry, exit {exit_code}"
            );
        }
    }

    /// Some runtimes decline to start an exec whose executable is not in the
    /// image rather than running it and reporting 127. That is still the
    /// image's business, not a container `dev exec` cannot reach.
    #[tokio::test(start_paused = true)]
    async fn up_tolerates_a_runtime_that_refuses_a_missing_executable() {
        let workspace = TempDir::new().unwrap();
        write_project_config(&workspace, r#"{"image":"scratch"}"#);
        let rt = UpFakeRuntime::has_no_shell();
        run_up_with_fake(&rt, &workspace)
            .await
            .expect("a missing shell must be reported, not fail readiness");
    }

    /// A runtime that shortens ids (as Apple Containers does, to fit its
    /// 36-character limit) must still count as discovered.
    #[test]
    fn same_container_tolerates_shortened_ids() {
        assert!(super::same_container("abc123def456", "abc123def456"));
        assert!(super::same_container("abc123def456", "abc123"));
        assert!(super::same_container("abc123", "abc123def456"));
        assert!(!super::same_container("abc123", "xyz789"));
        assert!(!super::same_container("", "abc123"));
        assert!(!super::same_container("abc123", ""));
    }
}
