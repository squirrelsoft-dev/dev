//! Common test utilities and fixtures

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Get BuildKit address from environment or use default
pub fn get_buildkit_addr() -> String {
    env::var("BUILDKIT_ADDR").unwrap_or_else(|_| "http://localhost:1234".to_string())
}

/// Check if BuildKit is available (for integration tests)
pub async fn is_buildkit_available() -> bool {
    use buildkit_client::BuildKitClient;

    let addr = get_buildkit_addr();
    match BuildKitClient::connect(&addr).await {
        Ok(mut client) => client.health_check().await.is_ok(),
        Err(_) => false,
    }
}

/// Create a temporary directory for tests
pub fn create_temp_dir(name: &str) -> PathBuf {
    let temp = env::temp_dir().join(format!("buildkit-test-{}", name));
    let _ = fs::create_dir_all(&temp);
    temp
}

/// Create a simple test Dockerfile
pub fn create_test_dockerfile(dir: &Path, content: Option<&str>) -> PathBuf {
    let dockerfile = dir.join("Dockerfile");
    let default_content = r#"FROM alpine:latest
RUN echo "Hello from test Dockerfile"
CMD ["echo", "test"]
"#;
    fs::write(&dockerfile, content.unwrap_or(default_content)).unwrap();
    dockerfile
}

/// Create a multi-stage test Dockerfile
pub fn create_multistage_dockerfile(dir: &Path) -> PathBuf {
    let dockerfile = dir.join("Dockerfile");
    let content = r#"FROM alpine:latest AS builder
RUN echo "Building..."
RUN echo "build output" > /build.txt

FROM alpine:latest AS production
COPY --from=builder /build.txt /app/build.txt
CMD ["cat", "/app/build.txt"]
"#;
    fs::write(&dockerfile, content).unwrap();
    dockerfile
}

/// Create a Dockerfile with build args
pub fn create_dockerfile_with_args(dir: &Path) -> PathBuf {
    let dockerfile = dir.join("Dockerfile");
    let content = r#"FROM alpine:latest
ARG VERSION=unknown
ARG BUILD_DATE=unknown
RUN echo "Version: $VERSION" > /version.txt
RUN echo "Build Date: $BUILD_DATE" >> /version.txt
CMD ["cat", "/version.txt"]
"#;
    fs::write(&dockerfile, content).unwrap();
    dockerfile
}

/// Create a test context with multiple files
pub fn create_test_context(dir: &Path) -> PathBuf {
    create_test_dockerfile(dir, None);

    let app_dir = dir.join("app");
    fs::create_dir_all(&app_dir).unwrap();

    fs::write(app_dir.join("main.txt"), "main content").unwrap();
    fs::write(app_dir.join("config.txt"), "config content").unwrap();

    let sub_dir = app_dir.join("subdir");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(sub_dir.join("data.txt"), "nested data").unwrap();

    dir.to_path_buf()
}

/// Cleanup temporary directory
pub fn cleanup_temp_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Skip test if BuildKit is not available
#[macro_export]
macro_rules! skip_without_buildkit {
    () => {
        if !common::is_buildkit_available().await {
            eprintln!(
                "Skipping test: BuildKit is not available at {}",
                common::get_buildkit_addr()
            );
            eprintln!("Set BUILDKIT_ADDR environment variable to specify BuildKit address");
            return;
        }
    };
}

/// Skip test if PAT_TOKEN environment variable is not set
#[macro_export]
macro_rules! skip_without_pat_token {
    () => {
        if std::env::var("PAT_TOKEN").is_err() {
            eprintln!("Skipping test: PAT_TOKEN environment variable is not set");
            return;
        }
    };
}

/// Test integration with environment variables
pub fn test_integration_with_env() {
    use dotenv;

    dotenv::dotenv().ok();
}

/// Create a .dockerignore file
pub fn create_dockerignore(dir: &Path, patterns: &[&str]) {
    let dockerignore = dir.join(".dockerignore");
    fs::write(&dockerignore, patterns.join("\n")).unwrap();
}

/// Generate a random test tag
pub fn random_test_tag() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let random: u32 = rng.gen();
    format!("buildkit-test:tag-{}", random)
}

/// Assert directory exists and contains expected files
pub fn assert_directory_structure(dir: &Path, expected_files: &[&str]) {
    assert!(dir.exists(), "Directory {} does not exist", dir.display());

    for file in expected_files {
        let path = dir.join(file);
        assert!(
            path.exists(),
            "Expected file {} does not exist",
            path.display()
        );
    }
}

/// Create a test file with specific content
pub fn create_test_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let file_path = dir.join(name);
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&file_path, content).unwrap();
    file_path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_temp_dir() {
        let dir = create_temp_dir("test");
        assert!(dir.exists());
        cleanup_temp_dir(&dir);
        assert!(!dir.exists());
    }

    #[test]
    fn test_create_test_dockerfile() {
        let dir = create_temp_dir("dockerfile-test");
        let dockerfile = create_test_dockerfile(&dir, None);
        assert!(dockerfile.exists());

        let content = fs::read_to_string(&dockerfile).unwrap();
        assert!(content.contains("FROM alpine:latest"));

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn test_create_test_context() {
        let dir = create_temp_dir("context-test");
        create_test_context(&dir);

        assert_directory_structure(
            &dir,
            &[
                "Dockerfile",
                "app/main.txt",
                "app/config.txt",
                "app/subdir/data.txt",
            ],
        );

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn test_random_test_tag() {
        let tag1 = random_test_tag();
        let tag2 = random_test_tag();

        assert!(tag1.starts_with("buildkit-test:tag-"));
        assert!(tag2.starts_with("buildkit-test:tag-"));
        assert_ne!(tag1, tag2);
    }
}
