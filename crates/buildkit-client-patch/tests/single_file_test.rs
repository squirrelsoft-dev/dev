//! Test with single file only (no directories)

mod common;

use buildkit_client::{BuildConfig, BuildKitClient};
use common::*;
use std::fs;

#[tokio::test]
async fn test_single_file_only() {
    skip_without_buildkit!();

    let test_dir = create_temp_dir("single-file");

    // Only Dockerfile, no other files or directories
    let dockerfile_content = r#"FROM alpine:latest
RUN echo "test"
"#;
    fs::write(test_dir.join("Dockerfile"), dockerfile_content).unwrap();

    eprintln!("\n=== Single file test (only Dockerfile) ===");

    let addr = get_buildkit_addr();
    let mut client = BuildKitClient::connect(&addr).await.unwrap();

    let config = BuildConfig::local(&test_dir);

    let result = client.build(config, None).await;

    cleanup_temp_dir(&test_dir);

    assert!(
        result.is_ok(),
        "Single file build failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn test_root_level_files_only() {
    skip_without_buildkit!();

    let test_dir = create_temp_dir("root-files");

    // Dockerfile + one file in root (no subdirectories)
    let dockerfile_content = r#"FROM alpine:latest
COPY test.txt /test.txt
RUN cat /test.txt
"#;
    fs::write(test_dir.join("Dockerfile"), dockerfile_content).unwrap();
    fs::write(test_dir.join("test.txt"), "test content").unwrap();

    eprintln!("\n=== Root level files test ===");

    let addr = get_buildkit_addr();
    let mut client = BuildKitClient::connect(&addr).await.unwrap();

    let config = BuildConfig::local(&test_dir);

    let result = client.build(config, None).await;

    cleanup_temp_dir(&test_dir);

    assert!(
        result.is_ok(),
        "Root level files build failed: {:?}",
        result.err()
    );
}
