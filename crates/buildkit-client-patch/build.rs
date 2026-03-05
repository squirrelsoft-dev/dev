use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Default repository URLs
const DEFAULT_BUILDKIT_REPO: &str = "https://github.com/moby/buildkit.git";
const DEFAULT_BUILDKIT_REF: &str = "master";
const DEFAULT_GOOGLEAPIS_REPO: &str = "https://github.com/googleapis/googleapis.git";
const DEFAULT_GOOGLEAPIS_REF: &str = "master";
const DEFAULT_PROTO_REBUILD: &str = "false";

// Proto file lists
const BUILDKIT_PROTOS: &[&str] = &[
    // API
    "api/services/control/control.proto",
    "api/types/worker.proto",
    // Solver
    "solver/pb/ops.proto",
    "solver/errdefs/errdefs.proto",
    // Source policy
    "sourcepolicy/pb/policy.proto",
    "sourcepolicy/policysession/policysession.proto",
    // Frontend
    "frontend/gateway/pb/gateway.proto",
    // Util
    "util/apicaps/pb/caps.proto",
    // Session
    "session/auth/auth.proto",
    "session/secrets/secrets.proto",
    "session/sshforward/ssh.proto",
    "session/filesync/filesync.proto",
    "session/upload/upload.proto",
    "session/exporter/exporter.proto",
];

// Vendor file mappings (source path in BuildKit repo -> destination path in proto dir)
const VENDOR_MAPPINGS: &[(&str, &str)] = &[
    // fsutil files
    (
        "vendor/github.com/tonistiigi/fsutil/types/stat.proto",
        "github.com/tonistiigi/fsutil/types/stat.proto",
    ),
    (
        "vendor/github.com/tonistiigi/fsutil/types/wire.proto",
        "github.com/tonistiigi/fsutil/types/wire.proto",
    ),
    // vtprotobuf files
    (
        "vendor/github.com/planetscale/vtprotobuf/vtproto/ext.proto",
        "github.com/planetscale/vtprotobuf/vtproto/ext.proto",
    ),
    // containerd files
    (
        "vendor/github.com/containerd/containerd/api/types/descriptor.proto",
        "github.com/containerd/containerd/api/types/descriptor.proto",
    ),
    (
        "vendor/github.com/containerd/containerd/api/types/platform.proto",
        "github.com/containerd/containerd/api/types/platform.proto",
    ),
    (
        "vendor/github.com/containerd/containerd/api/types/mount.proto",
        "github.com/containerd/containerd/api/types/mount.proto",
    ),
];

// Google RPC proto files
const GOOGLE_RPC_PROTOS: &[&str] = &[
    "google/rpc/status.proto",
    "google/rpc/code.proto",
    "google/rpc/error_details.proto",
];

#[derive(Debug, Clone, PartialEq)]
enum FetchMode {
    /// Download file content directly from raw.githubusercontent.com
    Content,
    /// Clone the entire repository using git
    Clone,
}

impl FetchMode {
    fn from_env() -> Self {
        match env::var("PROTO_FETCH_MODE").as_deref() {
            Ok("clone") => FetchMode::Clone,
            // Default to content mode for faster builds
            _ => FetchMode::Content,
        }
    }
}

#[derive(Debug, Clone)]
struct ProtoConfig {
    buildkit_repo: String,
    buildkit_ref: String,
    googleapis_repo: String,
    googleapis_ref: String,
    proto_dir: PathBuf,
    force_rebuild: bool,
    fetch_mode: FetchMode,
}

impl ProtoConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let buildkit_repo =
            env::var("BUILDKIT_REPO").unwrap_or_else(|_| DEFAULT_BUILDKIT_REPO.to_string());
        let buildkit_ref =
            env::var("BUILDKIT_REF").unwrap_or_else(|_| DEFAULT_BUILDKIT_REF.to_string());
        let googleapis_repo =
            env::var("GOOGLEAPIS_REPO").unwrap_or_else(|_| DEFAULT_GOOGLEAPIS_REPO.to_string());
        let googleapis_ref =
            env::var("GOOGLEAPIS_REF").unwrap_or_else(|_| DEFAULT_GOOGLEAPIS_REF.to_string());
        let force_rebuild = env::var("PROTO_REBUILD")
            .unwrap_or_else(|_| DEFAULT_PROTO_REBUILD.to_string())
            == "true";

        // Use OUT_DIR for proto files instead of source directory
        let out_dir = PathBuf::from(env::var("OUT_DIR")?);
        let proto_dir = out_dir.join("proto");
        let fetch_mode = FetchMode::from_env();

        Ok(ProtoConfig {
            buildkit_repo,
            buildkit_ref,
            googleapis_repo,
            googleapis_ref,
            proto_dir,
            force_rebuild,
            fetch_mode,
        })
    }

    /// Generate raw GitHub URL for BuildKit files
    fn get_buildkit_raw_url(&self, file_path: &str) -> String {
        let repo_parts = self
            .buildkit_repo
            .trim_end_matches(".git")
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/")
            .trim_start_matches("git@github.com:")
            .replace(".git", "");

        format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            repo_parts, self.buildkit_ref, file_path
        )
    }

    /// Generate raw GitHub URL for GoogleAPIs files
    fn get_googleapis_raw_url(&self, file_path: &str) -> String {
        let repo_parts = self
            .googleapis_repo
            .trim_end_matches(".git")
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/")
            .trim_start_matches("git@github.com:")
            .replace(".git", "");

        format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            repo_parts, self.googleapis_ref, file_path
        )
    }
}

#[derive(Debug, Default)]
struct FetchStats {
    copied: usize,
    missing: usize,
    downloaded: usize,
}

impl FetchStats {
    fn merge(&mut self, other: &FetchStats) {
        self.copied += other.copied;
        self.missing += other.missing;
        self.downloaded += other.downloaded;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.cargo/config.toml");

    let config = ProtoConfig::from_env()?;

    println!("Initializing proto files...");
    println!(
        "  BuildKit: {} @ {}",
        config.buildkit_repo, config.buildkit_ref
    );
    println!(
        "  GoogleAPIs: {} @ {}",
        config.googleapis_repo, config.googleapis_ref
    );
    println!("  Fetch mode: {:?}", config.fetch_mode);

    // Create proto directory
    fs::create_dir_all(&config.proto_dir)?;

    let mut total_stats = FetchStats::default();

    // Fetch BuildKit protos
    let stats = fetch_buildkit_protos(&config)?;
    total_stats.merge(&stats);

    // Fetch vendor protos
    let stats = fetch_vendor_protos(&config)?;
    total_stats.merge(&stats);

    // Create vtprotobuf stub if needed
    create_vtprotobuf_stub(&config)?;

    // Fetch Google APIs protos
    let stats = fetch_googleapis_protos(&config)?;
    total_stats.merge(&stats);

    // Print summary
    print_summary(&config, &total_stats);

    // Compile proto files with tonic-build
    compile_protos()?;

    Ok(())
}

/// Fetch BuildKit proto files
fn fetch_buildkit_protos(config: &ProtoConfig) -> Result<FetchStats, Box<dyn std::error::Error>> {
    let buildkit_target_dir = config.proto_dir.join("github.com/moby/buildkit");

    println!("\nFetching buildkit proto files to github.com/moby/buildkit/...");

    let mut stats = FetchStats::default();

    match config.fetch_mode {
        FetchMode::Content => {
            // Direct download mode using reqwest
            for proto in BUILDKIT_PROTOS {
                let url = config.get_buildkit_raw_url(proto);
                let dest_path = buildkit_target_dir.join(proto);

                match download_file(&url, &dest_path) {
                    Ok(()) => {
                        println!("  ✓ Downloaded {}", proto);
                        stats.downloaded += 1;
                    }
                    Err(e) => {
                        eprintln!("  ✗ {} (error: {})", proto, e);
                        stats.missing += 1;
                    }
                }
            }
        }
        FetchMode::Clone => {
            // Clone repository mode
            let buildkit_clone_dir = config.proto_dir.join(".buildkit");

            ensure_repository(
                &config.buildkit_repo,
                &config.buildkit_ref,
                &buildkit_clone_dir,
                config.force_rebuild,
            )?;

            for proto in BUILDKIT_PROTOS {
                match copy_proto(&buildkit_clone_dir, proto, &buildkit_target_dir) {
                    Ok(true) => stats.copied += 1,
                    Ok(false) => stats.missing += 1,
                    Err(e) => {
                        eprintln!("  ✗ {} (error: {})", proto, e);
                        stats.missing += 1;
                    }
                }
            }
        }
    }

    Ok(stats)
}

/// Fetch vendor proto files from BuildKit's vendor directory
fn fetch_vendor_protos(config: &ProtoConfig) -> Result<FetchStats, Box<dyn std::error::Error>> {
    println!("\nFetching vendor proto files...");

    let mut stats = FetchStats::default();

    match config.fetch_mode {
        FetchMode::Content => {
            // Direct download mode using reqwest
            for (src_path, dest_path) in VENDOR_MAPPINGS {
                let url = config.get_buildkit_raw_url(src_path);
                let dest = config.proto_dir.join(dest_path);

                match download_file(&url, &dest) {
                    Ok(()) => {
                        println!("  ✓ Downloaded {}", dest_path);
                        stats.downloaded += 1;
                    }
                    Err(e) => {
                        eprintln!("  ✗ {} (error: {})", dest_path, e);
                        stats.missing += 1;
                    }
                }
            }
        }
        FetchMode::Clone => {
            // Clone repository mode
            let buildkit_clone_dir = config.proto_dir.join(".buildkit");

            for (src_path, dest_path) in VENDOR_MAPPINGS {
                let src = buildkit_clone_dir.join(src_path);
                let dest = config.proto_dir.join(dest_path);

                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }

                if src.exists() {
                    fs::copy(&src, &dest)?;
                    println!("  ✓ {}", dest_path);
                    stats.copied += 1;
                } else {
                    println!("  ✗ {} (not found)", dest_path);
                    stats.missing += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Fetch Google APIs proto files
fn fetch_googleapis_protos(config: &ProtoConfig) -> Result<FetchStats, Box<dyn std::error::Error>> {
    println!("\nFetching google.rpc proto files...");

    let mut stats = FetchStats::default();

    match config.fetch_mode {
        FetchMode::Content => {
            // Direct download mode using reqwest
            for proto in GOOGLE_RPC_PROTOS {
                let url = config.get_googleapis_raw_url(proto);
                let dest = config.proto_dir.join(proto);

                match download_file(&url, &dest) {
                    Ok(()) => {
                        println!("  ✓ Downloaded {}", proto);
                        stats.downloaded += 1;
                    }
                    Err(e) => {
                        eprintln!("  ✗ {} (error: {})", proto, e);
                        stats.missing += 1;
                    }
                }
            }
        }
        FetchMode::Clone => {
            // Clone repository mode
            let googleapis_clone_dir = config.proto_dir.join(".googleapis");

            ensure_repository(
                &config.googleapis_repo,
                &config.googleapis_ref,
                &googleapis_clone_dir,
                config.force_rebuild,
            )?;

            for proto in GOOGLE_RPC_PROTOS {
                let src = googleapis_clone_dir.join(proto);
                let dest = config.proto_dir.join(proto);

                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }

                if src.exists() {
                    fs::copy(&src, &dest)?;
                    println!("  ✓ {}", proto);
                    stats.copied += 1;
                } else {
                    println!("  ✗ {} (not found)", proto);
                    stats.missing += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Download a file from URL to destination path using reqwest
fn download_file(url: &str, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Check if file exists and skip if not forced
    if dest.exists() && !should_rebuild() {
        return Ok(());
    }

    // Create parent directory if needed
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Use reqwest to download the file
    let client = reqwest::blocking::Client::builder()
        .user_agent("buildkit-client-build-script")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client.get(url).send()?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to download {} - HTTP status: {}",
            url,
            response.status()
        )
        .into());
    }

    let content = response.bytes()?;

    if content.is_empty() {
        return Err(format!("Empty response from {}", url).into());
    }

    fs::write(dest, content)?;

    Ok(())
}

/// Check if we should rebuild/redownload
fn should_rebuild() -> bool {
    env::var("PROTO_REBUILD").unwrap_or_else(|_| DEFAULT_PROTO_REBUILD.to_string()) == "true"
}

/// Create vtprotobuf stub file if it doesn't exist
fn create_vtprotobuf_stub(config: &ProtoConfig) -> Result<(), Box<dyn std::error::Error>> {
    let ext_proto = config
        .proto_dir
        .join("github.com/planetscale/vtprotobuf/vtproto/ext.proto");

    if !ext_proto.exists() {
        println!("\nCreating stub vtprotobuf/ext.proto...");
        fs::create_dir_all(ext_proto.parent().unwrap())?;
        fs::write(
            &ext_proto,
            r#"syntax = "proto3";
package vtproto;
option go_package = "github.com/planetscale/vtprotobuf/vtproto";
import "google/protobuf/descriptor.proto";
extend google.protobuf.MessageOptions {
  bool mempool = 65001;
}
"#,
        )?;
        println!("  ✓ Created stub ext.proto");
    }

    Ok(())
}

/// Ensure a repository is cloned and at the correct ref
fn ensure_repository(
    repo_url: &str,
    git_ref: &str,
    clone_dir: &Path,
    force_rebuild: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ref_file = clone_dir.join(".git_ref");

    let should_clone = force_rebuild || !clone_dir.exists() || !check_git_ref(&ref_file, git_ref)?;

    if should_clone {
        if clone_dir.exists() {
            println!("Removing old clone at {}...", clone_dir.display());
            fs::remove_dir_all(clone_dir)?;
        }

        println!("Cloning {} @ {}...", repo_url, git_ref);
        clone_repository(repo_url, git_ref, clone_dir)?;

        // Save the ref for future checks
        fs::write(&ref_file, git_ref)?;
    } else {
        println!("Using existing clone @ {}", git_ref);
    }

    Ok(())
}

/// Clone a git repository at a specific ref
fn clone_repository(
    repo_url: &str,
    git_ref: &str,
    dest: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // First try to clone with depth 1 and specific branch/tag
    let output = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            git_ref,
            repo_url,
            dest.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        // If branch/tag clone fails, try as commit hash
        println!("  Branch/tag clone failed, trying as commit...");

        // Clone without branch
        let output = Command::new("git")
            .args(["clone", repo_url, dest.to_str().unwrap()])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "Failed to clone repository: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        // Checkout specific commit
        let output = Command::new("git")
            .current_dir(dest)
            .args(["checkout", git_ref])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "Failed to checkout {}: {}",
                git_ref,
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
    }

    Ok(())
}

/// Check if the current git ref matches the expected one
fn check_git_ref(ref_file: &Path, expected_ref: &str) -> Result<bool, Box<dyn std::error::Error>> {
    if !ref_file.exists() {
        return Ok(false);
    }

    let saved_ref = fs::read_to_string(ref_file)?;
    Ok(saved_ref.trim() == expected_ref)
}

/// Copy a single proto file with its directory structure
fn copy_proto(
    src_base: &Path,
    src_file: &str,
    dest_base: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let src_path = src_base.join(src_file);
    let dest_path = dest_base.join(src_file);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if src_path.exists() {
        fs::copy(&src_path, &dest_path)?;
        println!("  ✓ {}", src_file);
        Ok(true)
    } else {
        println!("  ✗ {} (not found)", src_file);
        Ok(false)
    }
}

/// Print summary of the fetch operation
fn print_summary(config: &ProtoConfig, stats: &FetchStats) {
    println!("\n{}", "=".repeat(60));
    println!("Proto files initialization completed!");
    println!("{}", "=".repeat(60));
    println!("Configuration:");
    println!("  - BuildKit version: {}", config.buildkit_ref);
    println!("  - GoogleAPIs version: {}", config.googleapis_ref);
    println!("  - Fetch mode: {:?}", config.fetch_mode);
    println!("\nResults:");
    if stats.downloaded > 0 {
        println!("  - Downloaded: {} files", stats.downloaded);
    }
    if stats.copied > 0 {
        println!("  - Copied: {} files", stats.copied);
    }
    if stats.missing > 0 {
        println!("  - Missing: {} files", stats.missing);
    }
    println!("\nDirectory structure:");
    println!("  proto/");
    println!("  ├── github.com/");
    println!("  │   ├── moby/buildkit/");
    println!("  │   ├── tonistiigi/fsutil/");
    println!("  │   ├── planetscale/vtprotobuf/");
    println!("  │   └── containerd/containerd/");
    println!("  └── google/rpc/");
    println!("{}", "=".repeat(60));
}

/// Compile proto files using tonic-build
fn compile_protos() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let proto_dir = out_dir.join("proto");

    println!("\nCompiling proto files with tonic-build...");

    // Configure tonic-build
    tonic_build::configure()
        .build_server(true) // We need server for session services
        .build_client(true)
        .out_dir(&out_dir)
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::prost_types")
        .compile_protos(
            &[
                proto_dir.join("github.com/moby/buildkit/api/services/control/control.proto"),
                proto_dir.join("github.com/moby/buildkit/session/filesync/filesync.proto"),
                proto_dir.join("github.com/moby/buildkit/session/auth/auth.proto"),
                proto_dir.join("github.com/moby/buildkit/session/secrets/secrets.proto"),
            ],
            &[&proto_dir], // Include path
        )?;

    println!("✓ Proto compilation completed successfully");

    Ok(())
}
