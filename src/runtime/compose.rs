use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::devcontainer::features::MergedCapabilities;
use crate::error::DevError;

/// Determine the compose command based on the runtime name.
fn compose_cmd(runtime_name: &str) -> (&'static str, &'static str) {
    match runtime_name {
        "podman" => ("podman", "compose"),
        _ => ("docker", "compose"),
    }
}

/// Build compose file `-f` arguments from a list of paths.
/// Absolute paths are used as-is; relative paths are joined with `project_dir`.
fn compose_file_args(compose_files: &[&str], project_dir: &Path) -> Vec<String> {
    let mut args = Vec::new();
    for f in compose_files {
        args.push("-f".to_string());
        let path = Path::new(f);
        if path.is_absolute() {
            args.push(f.to_string());
        } else {
            args.push(project_dir.join(f).to_string_lossy().to_string());
        }
    }
    args
}

/// Build services defined in compose files.
pub async fn compose_build(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    service: Option<&str>,
    no_cache: bool,
    verbose: bool,
    env: &HashMap<String, String>,
) -> Result<(), DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(sub).args(&file_args).arg("build");
    for (k, v) in env {
        cmd.env(k, v);
    }
    if no_cache {
        cmd.arg("--no-cache");
    }
    if let Some(svc) = service {
        cmd.arg(svc);
    }

    if verbose {
        cmd.stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
    }

    let status = cmd.status().await.map_err(|e| {
        DevError::Runtime(format!("Failed to run {bin} {sub} build: {e}"))
    })?;
    if !status.success() {
        return Err(DevError::BuildFailed(format!(
            "{bin} {sub} build failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Start services in detached mode.
pub async fn compose_up(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    project_name: &str,
    env: &HashMap<String, String>,
    verbose: bool,
) -> Result<(), DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(sub)
        .args(&file_args)
        .arg("--project-name")
        .arg(project_name)
        .arg("up")
        .arg("-d");

    for (k, v) in env {
        cmd.env(k, v);
    }

    if verbose {
        cmd.stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
    }

    let status = cmd.status().await.map_err(|e| {
        DevError::Runtime(format!("Failed to run {bin} {sub} up: {e}"))
    })?;
    if !status.success() {
        return Err(DevError::Runtime(format!(
            "{bin} {sub} up failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Stop compose services without removing them.
pub async fn compose_stop(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    project_name: &str,
) -> Result<(), DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let status = tokio::process::Command::new(bin)
        .arg(sub)
        .args(&file_args)
        .arg("--project-name")
        .arg(project_name)
        .arg("stop")
        .status()
        .await
        .map_err(|e| DevError::Runtime(format!("Failed to run {bin} {sub} stop: {e}")))?;
    if !status.success() {
        return Err(DevError::Runtime(format!(
            "{bin} {sub} stop failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Stop and remove compose services.
pub async fn compose_down(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    project_name: &str,
) -> Result<(), DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let status = tokio::process::Command::new(bin)
        .arg(sub)
        .args(&file_args)
        .arg("--project-name")
        .arg(project_name)
        .arg("down")
        .status()
        .await
        .map_err(|e| DevError::Runtime(format!("Failed to run {bin} {sub} down: {e}")))?;
    if !status.success() {
        return Err(DevError::Runtime(format!(
            "{bin} {sub} down failed (exit {})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Get the container ID of a specific service.
pub async fn compose_container_id(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    project_name: &str,
    service: &str,
) -> Result<String, DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let output = tokio::process::Command::new(bin)
        .arg(sub)
        .args(&file_args)
        .arg("--project-name")
        .arg(project_name)
        .arg("ps")
        .arg("-q")
        .arg(service)
        .output()
        .await
        .map_err(|e| DevError::Runtime(format!("Failed to run {bin} {sub} ps: {e}")))?;

    if !output.status.success() {
        return Err(DevError::Runtime(format!(
            "Failed to get container ID for service '{service}'"
        )));
    }

    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        return Err(DevError::ContainerNotFound(format!(
            "No running container for compose service '{service}'"
        )));
    }
    Ok(id)
}

/// Get the image name for a composed service after building.
///
/// Runs `docker compose config --format json` and parses the service's image
/// field. Falls back to the Docker Compose default naming convention
/// (`<project_name>-<service_name>`) for build-based services.
pub async fn compose_service_image(
    runtime_name: &str,
    compose_files: &[&str],
    project_dir: &Path,
    project_name: &str,
    service: &str,
    env: &HashMap<String, String>,
) -> Result<String, DevError> {
    let (bin, sub) = compose_cmd(runtime_name);
    let file_args = compose_file_args(compose_files, project_dir);

    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(sub)
        .args(&file_args)
        .arg("--project-name")
        .arg(project_name)
        .arg("config")
        .arg("--format")
        .arg("json");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output()
        .await
        .map_err(|e| DevError::Runtime(format!("Failed to run {bin} {sub} config: {e}")))?;

    if output.status.success()
        && let Ok(config) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(image) = config
            .get("services")
            .and_then(|s| s.get(service))
            .and_then(|s| s.get("image"))
            .and_then(|i| i.as_str())
    {
        return Ok(image.to_string());
    }

    // Fallback: Docker Compose default naming convention for built images.
    Ok(format!("{project_name}-{service}"))
}

/// Convert a devcontainer mount string to a Compose volume entry.
///
/// Supports short form (`/host:/container[:ro]`) and long form
/// (`source=X,target=Y,type=bind[,readonly]`).
fn mount_to_compose_volume(mount_str: &str) -> Option<serde_json::Value> {
    let s = mount_str.trim();

    // Short form: /host:/container[:ro]
    if s.starts_with('/') || s.starts_with('.') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() >= 2 {
            let readonly = parts.get(2).is_some_and(|&p| p == "ro");
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), json!("bind"));
            entry.insert("source".into(), json!(parts[0]));
            entry.insert("target".into(), json!(parts[1]));
            if readonly {
                entry.insert("read_only".into(), json!(true));
            }
            return Some(serde_json::Value::Object(entry));
        }
        return None;
    }

    // Long form: key=value pairs separated by commas
    let mut source = None;
    let mut target = None;
    let mut mount_type = "bind".to_string();
    let mut readonly = false;

    for part in s.split(',') {
        let part = part.trim();
        if let Some((key, val)) = part.split_once('=') {
            match key {
                "source" | "src" => source = Some(val.to_string()),
                "target" | "dst" | "destination" => target = Some(val.to_string()),
                "type" => mount_type = val.to_string(),
                "readonly" | "ro" => {
                    readonly = val.is_empty() || val == "true" || val == "1";
                }
                _ => {}
            }
        } else if part == "readonly" || part == "ro" {
            readonly = true;
        }
    }

    match (source, target) {
        (Some(src), Some(tgt)) => {
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), json!(mount_type));
            entry.insert("source".into(), json!(src));
            entry.insert("target".into(), json!(tgt));
            if readonly {
                entry.insert("read_only".into(), json!(true));
            }
            Some(serde_json::Value::Object(entry))
        }
        _ => None,
    }
}

/// Generate a Docker Compose override YAML that injects devcontainer properties
/// (labels, env, mounts, volumes, ports, image, feature capabilities) into the
/// target service.
#[allow(clippy::too_many_arguments)]
pub fn generate_compose_override(
    service: &str,
    labels: &[(String, String)],
    env: &HashMap<String, String>,
    mounts: &[String],
    volumes: &[String],
    ports: &[super::PortMapping],
    image: Option<&str>,
    caps: &MergedCapabilities,
) -> String {
    let mut service_obj = serde_json::Map::new();

    if let Some(img) = image {
        service_obj.insert("image".into(), json!(img));
    }

    if !labels.is_empty() {
        let labels_obj: serde_json::Map<_, _> = labels
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        service_obj.insert("labels".into(), serde_json::Value::Object(labels_obj));
    }

    if !env.is_empty() {
        let env_obj: serde_json::Map<_, _> = env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        service_obj.insert("environment".into(), serde_json::Value::Object(env_obj));
    }

    let mut vol_entries = Vec::new();
    let mut top_level_volumes = serde_json::Map::new();

    for mount_str in mounts {
        if let Some(entry) = mount_to_compose_volume(mount_str) {
            vol_entries.push(entry);
        }
    }

    for vol_str in volumes {
        let parts: Vec<&str> = vol_str.split(':').collect();
        if parts.len() >= 2 {
            let readonly = parts.get(2).is_some_and(|&p| p == "ro");
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), json!("volume"));
            entry.insert("source".into(), json!(parts[0]));
            entry.insert("target".into(), json!(parts[1]));
            if readonly {
                entry.insert("read_only".into(), json!(true));
            }
            vol_entries.push(serde_json::Value::Object(entry));
            top_level_volumes.insert(parts[0].to_string(), serde_json::Value::Null);
        }
    }

    if !vol_entries.is_empty() {
        service_obj.insert("volumes".into(), serde_json::Value::Array(vol_entries));
    }

    if !ports.is_empty() {
        let port_entries: Vec<serde_json::Value> = ports
            .iter()
            .map(|p| json!(format!("{}:{}", p.host, p.container)))
            .collect();
        service_obj.insert("ports".into(), serde_json::Value::Array(port_entries));
    }

    if caps.init {
        service_obj.insert("init".into(), json!(true));
    }
    if caps.privileged {
        service_obj.insert("privileged".into(), json!(true));
    }
    if !caps.cap_add.is_empty() {
        service_obj.insert(
            "cap_add".into(),
            serde_json::Value::Array(
                caps.cap_add.iter().map(|c| json!(c)).collect(),
            ),
        );
    }
    if !caps.security_opt.is_empty() {
        service_obj.insert(
            "security_opt".into(),
            serde_json::Value::Array(
                caps.security_opt.iter().map(|c| json!(c)).collect(),
            ),
        );
    }

    let mut services = serde_json::Map::new();
    services.insert(service.to_string(), serde_json::Value::Object(service_obj));

    let mut root = serde_json::Map::new();
    root.insert("services".into(), serde_json::Value::Object(services));

    if !top_level_volumes.is_empty() {
        root.insert("volumes".into(), serde_json::Value::Object(top_level_volumes));
    }

    serde_yaml::to_string(&serde_json::Value::Object(root)).unwrap_or_default()
}

/// Rewrite relative volume source paths in a compose file so they resolve
/// against the actual workspace instead of the compose file's directory.
///
/// Recipe-based configs store compose files under `~/.dev/devcontainers/`,
/// so `..` in a volume source resolves to the wrong directory.  This rewrites
/// sources that start with `..` to absolute paths, treating the compose file
/// as if it lived in `<workspace>/.devcontainer/`.
///
/// Returns the path to a temporary rewritten compose file.
pub fn rewrite_compose_volumes(
    compose_path: &Path,
    workspace: &Path,
) -> Result<PathBuf, DevError> {
    let content = std::fs::read_to_string(compose_path)
        .map_err(|e| DevError::Runtime(format!("Failed to read {}: {e}", compose_path.display())))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| DevError::Runtime(format!("Failed to parse {}: {e}", compose_path.display())))?;

    let workspace_abs = workspace.canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());

    if let Some(services) =
        doc.get_mut("services").and_then(|s| s.as_mapping_mut())
    {
        for (_name, service) in services.iter_mut() {
            rewrite_service_volumes(service, &workspace_abs);
        }
    }

    let rewritten = serde_yaml::to_string(&doc)
        .map_err(|e| DevError::Runtime(format!("Failed to serialize compose file: {e}")))?;

    let file_stem = compose_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("compose");
    let temp_path = std::env::temp_dir().join(format!(
        "dev-{}-rewritten-{}.yml",
        file_stem,
        std::process::id()
    ));
    std::fs::write(&temp_path, &rewritten)
        .map_err(|e| DevError::Runtime(format!("Failed to write rewritten compose file: {e}")))?;

    Ok(temp_path)
}

/// Rewrite volume source paths that start with `..` in a single service.
fn rewrite_service_volumes(service: &mut serde_yaml::Value, workspace: &Path) {
    let volumes = match service
        .get_mut("volumes")
        .and_then(|v| v.as_sequence_mut())
    {
        Some(seq) => seq,
        None => return,
    };

    for vol in volumes.iter_mut() {
        match vol {
            // Short syntax: "source:target[:mode]"
            serde_yaml::Value::String(s) => {
                if s.starts_with("..") {
                    if let Some(colon_pos) = s.find(':') {
                        let source = &s[..colon_pos];
                        let rest = &s[colon_pos..];
                        let resolved = resolve_parent_ref(source, workspace);
                        *s = format!("{}{rest}", resolved.display());
                    }
                }
            }
            // Long syntax: mapping with source/target/type keys
            serde_yaml::Value::Mapping(map) => {
                let source_key = serde_yaml::Value::String("source".to_string());
                if let Some(source_val) = map.get_mut(&source_key) {
                    if let Some(s) = source_val.as_str() {
                        if s.starts_with("..") {
                            let resolved = resolve_parent_ref(s, workspace);
                            *source_val = serde_yaml::Value::String(
                                resolved.to_string_lossy().to_string(),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Resolve a `..`-prefixed relative path as if the compose file were in
/// `<workspace>/.devcontainer/`.
///
/// - `..`        → workspace root
/// - `../foo`    → `<workspace>/foo`
fn resolve_parent_ref(rel_path: &str, workspace: &Path) -> PathBuf {
    if rel_path == ".." {
        workspace.to_path_buf()
    } else if let Some(rest) = rel_path.strip_prefix("../") {
        workspace.join(rest)
    } else {
        // e.g. `../../something` — do a simple component-based resolution
        let virtual_base = workspace.join(".devcontainer");
        normalize_path(&virtual_base.join(rel_path))
    }
}

/// Normalize a path by resolving `.` and `..` components without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => { parts.pop(); }
            Component::CurDir => {}
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// Write a compose override to a temporary file and return its path.
pub fn write_override_file(content: &str) -> Result<PathBuf, DevError> {
    let path = std::env::temp_dir().join(format!(
        "dev-compose-override-{}.yml",
        std::process::id()
    ));
    std::fs::write(&path, content).map_err(|e| {
        DevError::Runtime(format!("Failed to write compose override file: {e}"))
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::features::MergedCapabilities;

    #[test]
    fn test_compose_file_args_relative() {
        let args = compose_file_args(&["docker-compose.yml"], Path::new("/project/.devcontainer"));
        assert_eq!(args, vec!["-f", "/project/.devcontainer/docker-compose.yml"]);
    }

    #[test]
    fn test_compose_file_args_absolute() {
        let args = compose_file_args(
            &["docker-compose.yml", "/tmp/override.yml"],
            Path::new("/project/.devcontainer"),
        );
        assert_eq!(args, vec![
            "-f", "/project/.devcontainer/docker-compose.yml",
            "-f", "/tmp/override.yml",
        ]);
    }

    #[test]
    fn test_mount_to_compose_volume_short_form() {
        let result = mount_to_compose_volume("/host/path:/container/path").unwrap();
        assert_eq!(result["type"], "bind");
        assert_eq!(result["source"], "/host/path");
        assert_eq!(result["target"], "/container/path");
        assert!(result.get("read_only").is_none());
    }

    #[test]
    fn test_mount_to_compose_volume_short_form_readonly() {
        let result = mount_to_compose_volume("/host:/container:ro").unwrap();
        assert_eq!(result["type"], "bind");
        assert_eq!(result["read_only"], true);
    }

    #[test]
    fn test_mount_to_compose_volume_long_form() {
        let result = mount_to_compose_volume(
            "source=/host/path,target=/container/path,type=bind,readonly"
        ).unwrap();
        assert_eq!(result["type"], "bind");
        assert_eq!(result["source"], "/host/path");
        assert_eq!(result["target"], "/container/path");
        assert_eq!(result["read_only"], true);
    }

    #[test]
    fn test_mount_to_compose_volume_volume_type() {
        let result = mount_to_compose_volume(
            "source=my-vol,target=/data,type=volume"
        ).unwrap();
        assert_eq!(result["type"], "volume");
        assert_eq!(result["source"], "my-vol");
        assert_eq!(result["target"], "/data");
    }

    #[test]
    fn test_generate_compose_override_basic() {
        let labels = vec![
            ("devcontainer.local_folder".to_string(), "/workspace".to_string()),
        ];
        let mut env = HashMap::new();
        env.insert("SHELL".to_string(), "/bin/bash".to_string());
        let caps = MergedCapabilities::default();

        let yaml = generate_compose_override(
            "app", &labels, &env, &[], &[], &[], None, &caps,
        );
        assert!(yaml.contains("app:"));
        assert!(yaml.contains("devcontainer.local_folder"));
        assert!(yaml.contains("SHELL"));
    }

    #[test]
    fn test_generate_compose_override_with_image_and_caps() {
        let caps = MergedCapabilities {
            init: true,
            privileged: false,
            cap_add: vec!["SYS_PTRACE".to_string()],
            security_opt: vec!["seccomp=unconfined".to_string()],
        };

        let yaml = generate_compose_override(
            "web", &[], &HashMap::new(), &[], &[],
            &[crate::runtime::PortMapping { host: 3000, container: 3000 }],
            Some("myimage:featured"), &caps,
        );
        assert!(yaml.contains("myimage:featured"));
        assert!(yaml.contains("init: true"));
        assert!(yaml.contains("SYS_PTRACE"));
        assert!(yaml.contains("seccomp=unconfined"));
        assert!(yaml.contains("3000:3000"));
    }

    #[test]
    fn test_generate_compose_override_with_volumes() {
        let mounts = vec![
            "source=/home/user/.ssh,target=/home/vscode/.ssh,type=bind".to_string(),
        ];
        let volumes = vec!["extensions:/home/vscode/.vscode-server/extensions".to_string()];
        let caps = MergedCapabilities::default();

        let yaml = generate_compose_override(
            "dev", &[], &HashMap::new(), &mounts, &volumes, &[], None, &caps,
        );
        assert!(yaml.contains("/home/user/.ssh"));
        assert!(yaml.contains("extensions"));
        // Top-level volumes section should be present for named volumes.
        let lines: Vec<&str> = yaml.lines().collect();
        let top_level_volumes = lines.iter().any(|l| l == &"volumes:" || l.starts_with("volumes:"));
        assert!(top_level_volumes);
    }

    #[test]
    fn test_write_override_file() {
        let content = "services:\n  app:\n    image: test\n";
        let path = write_override_file(content).unwrap();
        assert!(path.exists());
        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back, content);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_parent_ref() {
        let ws = Path::new("/home/user/projects/myapp");
        assert_eq!(resolve_parent_ref("..", ws), PathBuf::from("/home/user/projects/myapp"));
        assert_eq!(resolve_parent_ref("../src", ws), PathBuf::from("/home/user/projects/myapp/src"));
    }

    #[test]
    fn test_rewrite_compose_volumes_short_syntax() {
        let dir = tempfile::TempDir::new().unwrap();
        let compose = dir.path().join("short-compose.yml");
        std::fs::write(&compose, r#"
services:
  app:
    image: node:20
    volumes:
      - ..:/workspaces/myapp:cached
      - ./scripts:/scripts
"#).unwrap();

        let workspace = Path::new("/home/user/projects/myapp");
        let rewritten_path = rewrite_compose_volumes(&compose, workspace).unwrap();
        let content = std::fs::read_to_string(&rewritten_path).unwrap();

        // `..` should be rewritten to the workspace path
        assert!(content.contains("/home/user/projects/myapp:/workspaces/myapp:cached"), "Got: {content}");
        // `./scripts` should NOT be rewritten (not a parent ref)
        assert!(content.contains("./scripts:/scripts"), "Got: {content}");

        let _ = std::fs::remove_file(&rewritten_path);
    }

    #[test]
    fn test_rewrite_compose_volumes_long_syntax() {
        let dir = tempfile::TempDir::new().unwrap();
        let compose = dir.path().join("long-compose.yml");
        std::fs::write(&compose, r#"
services:
  app:
    image: node:20
    volumes:
      - type: bind
        source: ..
        target: /workspaces/myapp
"#).unwrap();

        let workspace = Path::new("/home/user/projects/myapp");
        let rewritten_path = rewrite_compose_volumes(&compose, workspace).unwrap();
        let content = std::fs::read_to_string(&rewritten_path).unwrap();

        assert!(content.contains("/home/user/projects/myapp"), "Got: {content}");
        assert!(!content.contains("source: .."), "Got: {content}");

        let _ = std::fs::remove_file(&rewritten_path);
    }
}
