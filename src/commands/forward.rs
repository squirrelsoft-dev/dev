use std::path::Path;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use crate::runtime::{ContainerRuntime, ContainerState, detect_runtime};
use crate::util::paths::dev_home;
use crate::util::workspace_labels;

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    port_spec: &str,
    daemon: bool,
    stop: bool,
    list: bool,
) -> anyhow::Result<()> {
    if list {
        return list_forwarders(workspace);
    }

    let (host_port, container_port) = parse_port_spec(port_spec)?;

    if stop {
        return stop_forwarder(workspace, host_port);
    }

    if daemon {
        return daemonize(workspace, host_port);
    }

    run_forwarder(workspace, runtime_override, host_port, container_port).await
}

fn parse_port_spec(spec: &str) -> anyhow::Result<(u16, u16)> {
    if let Some((host, container)) = spec.split_once(':') {
        let h: u16 = host
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid host port: {host}"))?;
        let c: u16 = container
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid container port: {container}"))?;
        Ok((h, c))
    } else {
        let p: u16 = spec
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid port: {spec}"))?;
        Ok((p, p))
    }
}

// --- PID file helpers ---

fn forward_dir() -> std::path::PathBuf {
    dev_home().join("forward")
}

fn workspace_hash(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let abs = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(abs.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

fn pid_file_path(workspace: &Path, host_port: u16) -> std::path::PathBuf {
    forward_dir().join(format!("{}-{}.pid", workspace_hash(workspace), host_port))
}

fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn read_pid_file(path: &Path) -> anyhow::Result<u32> {
    let contents = std::fs::read_to_string(path)?;
    let pid: u32 = contents.trim().parse()?;
    Ok(pid)
}

/// Collect host ports from active PID files for this workspace.
fn active_ports_for_workspace(workspace: &Path) -> Vec<u16> {
    let dir = forward_dir();
    let prefix = workspace_hash(workspace);
    let mut ports = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return ports,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".pid") {
            continue;
        }
        let middle = &name[prefix.len() + 1..name.len() - 4];
        if let Ok(port) = middle.parse::<u16>() {
            if let Ok(pid) = read_pid_file(&entry.path()) {
                if is_process_alive(pid) {
                    ports.push(port);
                }
            }
        }
    }

    ports
}

// --- Modes ---

fn list_forwarders(workspace: &Path) -> anyhow::Result<()> {
    let dir = forward_dir();
    if !dir.is_dir() {
        eprintln!("No active forwarders.");
        return Ok(());
    }

    let prefix = workspace_hash(workspace);
    let mut found = false;

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !name.starts_with(&prefix) || !name.ends_with(".pid") {
            continue;
        }

        // Extract port from filename: {hash}-{port}.pid
        let middle = &name[prefix.len() + 1..name.len() - 4];
        let port: u16 = match middle.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let path = entry.path();
        match read_pid_file(&path) {
            Ok(pid) if is_process_alive(pid) => {
                if !found {
                    eprintln!("Active forwarders:");
                    found = true;
                }
                eprintln!("  localhost:{port} (PID {pid})");
            }
            _ => {
                // Stale PID file — clean up
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    if !found {
        eprintln!("No active forwarders.");
    }
    Ok(())
}

fn stop_forwarder(workspace: &Path, host_port: u16) -> anyhow::Result<()> {
    let path = pid_file_path(workspace, host_port);
    let pid = read_pid_file(&path)
        .map_err(|_| anyhow::anyhow!("No forwarder running on port {host_port}"))?;

    if !is_process_alive(pid) {
        let _ = std::fs::remove_file(&path);
        anyhow::bail!("Forwarder on port {host_port} (PID {pid}) is no longer running");
    }

    unsafe { libc::kill(pid as i32, libc::SIGTERM) };

    // Wait up to 2 seconds for graceful exit
    for _ in 0..20 {
        if !is_process_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if is_process_alive(pid) {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    let _ = std::fs::remove_file(&path);
    eprintln!("Stopped forwarder on port {host_port} (PID {pid})");

    // Update Caddy config: regenerate with remaining active ports or remove entirely
    let remaining_ports = active_ports_for_workspace(workspace);
    if remaining_ports.is_empty() {
        if let Err(e) = crate::caddy::unregister_site(workspace) {
            eprintln!("Warning: Caddy cleanup failed: {e}");
        }
    } else if let Err(e) = crate::caddy::register_site(workspace, &remaining_ports) {
        eprintln!("Warning: Caddy update failed: {e}");
    }

    Ok(())
}

fn daemonize(workspace: &Path, host_port: u16) -> anyhow::Result<()> {
    let pid_path = pid_file_path(workspace, host_port);

    // Check for existing forwarder
    if pid_path.is_file() {
        if let Ok(pid) = read_pid_file(&pid_path)
            && is_process_alive(pid)
        {
            anyhow::bail!(
                "Forwarder already running on port {host_port} (PID {pid}). \
                 Stop it with: dev forward {host_port} --stop"
            );
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    // Re-exec ourselves without -d, with stdout/stderr/stdin redirected to null
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args()
        .skip(1) // skip binary name
        .filter(|a| a != "-d" && a != "--daemon")
        .collect();

    std::fs::create_dir_all(forward_dir())?;

    let child = std::process::Command::new(exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let pid = child.id();
    std::fs::write(&pid_path, pid.to_string())?;
    eprintln!("Forwarding localhost:{host_port} in background (PID {pid})");
    eprintln!("Stop with: dev forward {host_port} --stop");
    Ok(())
}

async fn run_forwarder(
    workspace: &Path,
    runtime_override: Option<&str>,
    host_port: u16,
    container_port: u16,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| {
            anyhow::anyhow!("No running container found for this workspace. Run `dev up` first.")
        })?;

    let container_id = container.id.clone();

    // Verify netcat is available
    let nc_binary = find_netcat(runtime.as_ref(), &container_id).await?;

    let listener = TcpListener::bind(format!("127.0.0.1:{host_port}"))
        .await
        .map_err(|e| anyhow::anyhow!("Could not bind port {host_port}: {e}"))?;

    eprintln!(
        "Forwarding 127.0.0.1:{host_port} -> container:{container_port}"
    );

    let mut all_ports = active_ports_for_workspace(workspace);
    if !all_ports.contains(&host_port) {
        all_ports.push(host_port);
    }
    all_ports.sort();
    if let Err(e) = crate::caddy::register_site(workspace, &all_ports) {
        eprintln!("Warning: Caddy setup failed: {e}");
    }

    eprintln!("Press Ctrl+C to stop.");

    let runtime: Arc<dyn ContainerRuntime> = Arc::from(runtime);

    let accept_loop = async {
        loop {
            let (tcp_stream, peer) = listener.accept().await?;
            eprintln!("[forward] Connection from {peer}");

            let runtime = Arc::clone(&runtime);
            let container_id = container_id.clone();
            let nc_binary = nc_binary.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(
                    tcp_stream,
                    runtime.as_ref(),
                    &container_id,
                    container_port,
                    &nc_binary,
                )
                .await
                {
                    eprintln!("[forward] Connection error: {e}");
                }
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    tokio::select! {
        result = accept_loop => result?,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nStopping port forward.");
            if let Err(e) = crate::caddy::unregister_site(workspace) {
                eprintln!("Warning: Caddy cleanup failed: {e}");
            }
        }
    }

    Ok(())
}

async fn find_netcat(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
) -> anyhow::Result<String> {
    for name in &["nc", "ncat", "netcat"] {
        let result = runtime
            .exec(
                container_id,
                &["which".to_string(), name.to_string()],
                None,
            )
            .await?;
        if result.exit_code == 0 {
            return Ok(name.to_string());
        }
    }
    anyhow::bail!(
        "netcat (nc) is not installed in the container.\n\
         Install it with: apt-get install -y netcat-openbsd"
    )
}

async fn handle_connection(
    tcp_stream: tokio::net::TcpStream,
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    container_port: u16,
    nc_binary: &str,
) -> anyhow::Result<()> {
    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("{nc_binary} 127.0.0.1 {container_port}"),
    ];
    let attached = runtime.exec_attached(container_id, &cmd, None).await?;

    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();
    let mut exec_stdout = attached.stdout;
    let mut exec_stdin = attached.stdin;

    let container_to_tcp = async {
        tokio::io::copy(&mut exec_stdout, &mut tcp_write).await?;
        tcp_write.shutdown().await?;
        anyhow::Ok(())
    };

    let tcp_to_container = async {
        tokio::io::copy(&mut tcp_read, &mut exec_stdin).await?;
        exec_stdin.shutdown().await?;
        anyhow::Ok(())
    };

    tokio::select! {
        r = container_to_tcp => r?,
        r = tcp_to_container => r?,
    }

    Ok(())
}
