use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::paths::dev_home;

const TLD: &str = "test";

/// Root dir for dev-managed Caddy config: `~/.dev/caddy/`
fn caddy_dir() -> PathBuf {
    dev_home().join("caddy")
}

/// Main Caddyfile that imports all fragments: `~/.dev/caddy/Caddyfile`
fn caddyfile_path() -> PathBuf {
    caddy_dir().join("Caddyfile")
}

/// Per-project fragment: `~/.dev/caddy/sites/<app_name>.caddy`
fn site_config_path(app_name: &str) -> PathBuf {
    caddy_dir().join("sites").join(format!("{app_name}.caddy"))
}

/// Derive a DNS-safe hostname from a workspace path.
/// Uses the folder name, lowercased, non-alphanumeric chars replaced by dashes.
fn app_name_from_workspace(workspace: &Path) -> String {
    let dirname = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".to_string());

    dirname
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// A single port→hostname mapping.
#[derive(Debug, Clone)]
pub struct SiteEntry {
    pub hostname: String,
    pub host_port: u16,
}

/// A port with an optional custom hostname override.
#[derive(Debug, Clone)]
pub struct PortEntry {
    pub port: u16,
    pub custom_name: Option<String>,
    pub keepalive: Option<String>,
}

/// Write the Caddy config fragment for a project.
///
/// Each forwarded port gets its own subdomain:
///   - First port:  `myapp.test` → `127.0.0.1:3000`
///   - Extra ports: `myapp-8080.test` → `127.0.0.1:8080`
///   - Custom name: `admin.myapp.test` → `127.0.0.1:8080` (via --name)
fn write_site_config(app_name: &str, ports: &[PortEntry]) -> anyhow::Result<Vec<SiteEntry>> {
    let sites_dir = caddy_dir().join("sites");
    std::fs::create_dir_all(&sites_dir)?;

    let mut entries = Vec::new();
    let mut config = String::new();

    for (i, entry) in ports.iter().enumerate() {
        let hostname = if let Some(ref name) = entry.custom_name {
            name.clone()
        } else if i == 0 {
            format!("{app_name}.{TLD}")
        } else {
            format!("{app_name}-{}.{TLD}", entry.port)
        };

        let proxy_block = match &entry.keepalive {
            Some(k) => format!(
                "    reverse_proxy 127.0.0.1:{} {{\n        transport http {{\n            keepalive {}\n        }}\n    }}\n",
                entry.port, k
            ),
            None => format!("    reverse_proxy 127.0.0.1:{}\n", entry.port),
        };
        config.push_str(&format!(
            "{hostname} {{\n    tls internal\n{proxy_block}}}\n\n"
        ));

        entries.push(SiteEntry {
            hostname,
            host_port: entry.port,
        });
    }

    std::fs::write(site_config_path(app_name), &config)?;
    Ok(entries)
}

/// Remove a project's Caddy config fragment.
fn remove_site_config(app_name: &str) -> anyhow::Result<()> {
    let path = site_config_path(app_name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Write the root Caddyfile that imports all site fragments (idempotent).
fn ensure_root_caddyfile() -> anyhow::Result<()> {
    let path = caddyfile_path();
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(caddy_dir())?;
    std::fs::create_dir_all(caddy_dir().join("sites"))?;

    let content = format!(
        "# Managed by dev CLI — do not edit manually\nimport {}/sites/*.caddy\n",
        caddy_dir().display()
    );
    std::fs::write(&path, content)?;
    Ok(())
}

/// Signal Caddy to reload. Best-effort — prints hints if Caddy isn't available.
fn reload_caddy() {
    let which = Command::new("which").arg("caddy").output();
    if which.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!(
            "hint: install Caddy for automatic .{TLD} routing:\n  \
             brew install caddy\n  \
             sudo caddy start --config {}",
            caddyfile_path().display()
        );
        return;
    }

    let caddyfile = caddyfile_path();
    let result = Command::new("caddy")
        .args(["reload", "--config", &caddyfile.to_string_lossy()])
        .output();

    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("dial") || stderr.contains("connection refused") {
                eprintln!("Caddy not running, starting...");
                let _ = Command::new("caddy")
                    .args(["start", "--config", &caddyfile.to_string_lossy()])
                    .output();
            } else {
                eprintln!("Warning: caddy reload failed: {stderr}");
            }
        }
        Err(e) => {
            eprintln!("Warning: could not run caddy: {e}");
        }
    }
}

/// Check if dnsmasq resolver is configured for the TLD.
fn check_dns_setup() -> bool {
    Path::new(&format!("/etc/resolver/{TLD}")).exists()
}

/// One-time dnsmasq setup instructions.
///
/// Returned as a value rather than printed directly so the steps can be asserted
/// against the copy README.md documents; the two drifting apart is a real defect.
fn dns_setup_hint() -> String {
    format!(
        "\nhint: to enable .{TLD} routing, run once:\n  \
         brew install dnsmasq\n  \
         echo 'address=/.{TLD}/127.0.0.1' >> /opt/homebrew/etc/dnsmasq.conf\n  \
         sudo brew services start dnsmasq\n  \
         sudo mkdir -p /etc/resolver\n  \
         echo 'nameserver 127.0.0.1' | sudo tee /etc/resolver/{TLD}\n"
    )
}

/// Print one-time dnsmasq setup instructions.
fn print_dns_setup_hint() {
    eprintln!("{}", dns_setup_hint());
}

/// Register forwarded ports with Caddy. Best-effort — warns on failure.
pub fn register_site(workspace: &Path, ports: &[PortEntry]) -> anyhow::Result<Vec<SiteEntry>> {
    let app_name = app_name_from_workspace(workspace);
    ensure_root_caddyfile()?;
    let entries = write_site_config(&app_name, ports)?;
    reload_caddy();

    if !check_dns_setup() {
        print_dns_setup_hint();
    }

    for entry in &entries {
        eprintln!(
            "  → https://{} → localhost:{}",
            entry.hostname, entry.host_port
        );
    }

    Ok(entries)
}

/// Remove a project's Caddy config and reload.
pub fn unregister_site(workspace: &Path) -> anyhow::Result<()> {
    let app_name = app_name_from_workspace(workspace);
    remove_site_config(&app_name)?;
    reload_caddy();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DNS setup steps `dev up` prints must match the ones README.md documents.
    /// A reader who follows the README and a reader who follows the hint have to end
    /// up with the same working resolver.
    #[test]
    fn dns_setup_hint_steps_are_documented_in_readme() {
        let readme = include_str!("../README.md");
        let hint = dns_setup_hint();

        let steps: Vec<&str> = hint
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
            .collect();

        assert!(!steps.is_empty(), "hint produced no setup steps");
        for step in steps {
            assert!(
                readme.contains(step),
                "DNS setup step is printed by `dev up` but missing from README.md: {step}"
            );
        }
    }

    /// Writing the resolver file with `tee` fails if /etc/resolver does not exist yet,
    /// which is the default on a machine that has never configured one.
    #[test]
    fn dns_setup_hint_creates_resolver_dir_before_writing_to_it() {
        let hint = dns_setup_hint();
        let mkdir = hint
            .find("mkdir -p /etc/resolver")
            .expect("hint must create /etc/resolver");
        let tee = hint
            .find("tee /etc/resolver/")
            .expect("hint must write the resolver file");
        assert!(
            mkdir < tee,
            "hint must create /etc/resolver before writing into it"
        );
    }
}
