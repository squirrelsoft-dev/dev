//! Minimal test to debug context file transfer

mod common;

use buildkit_client::{BuildConfig, BuildKitClient};
use common::*;
use std::fs;

#[tokio::test]
async fn test_minimal_context() {
    skip_without_buildkit!();

    let test_dir = create_temp_dir("minimal-context");

    // Create minimal structure: just Dockerfile + one dir + one file
    let dockerfile_content = r#"FROM alpine:latest
COPY data/file.txt /file.txt
RUN cat /file.txt
"#;
    fs::write(test_dir.join("Dockerfile"), dockerfile_content).unwrap();

    let data_dir = test_dir.join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::write(data_dir.join("file.txt"), "test content").unwrap();

    eprintln!("\n=== Test directory structure ===");
    eprintln!("Dockerfile");
    eprintln!("data/ (dir)");
    eprintln!("data/file.txt");

    let addr = get_buildkit_addr();
    let mut client = BuildKitClient::connect(&addr).await.unwrap();

    let config = BuildConfig::local(&test_dir);

    let result = client.build(config, None).await;

    cleanup_temp_dir(&test_dir);

    assert!(
        result.is_ok(),
        "Minimal context build failed: {:?}",
        result.err()
    );
}
