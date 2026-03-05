//! Unit tests for BuildConfig and related types

use buildkit_client::{BuildConfig, DockerfileSource, Platform, RegistryAuth};
use std::path::PathBuf;

#[test]
fn test_platform_parse() {
    let platform = Platform::parse("linux/amd64").unwrap();
    assert_eq!(platform.os, "linux");
    assert_eq!(platform.arch, "amd64");
    assert_eq!(platform.variant, None);

    let platform = Platform::parse("linux/arm64/v8").unwrap();
    assert_eq!(platform.os, "linux");
    assert_eq!(platform.arch, "arm64");
    assert_eq!(platform.variant, Some("v8".to_string()));

    assert!(Platform::parse("invalid").is_err());
}

#[test]
fn test_platform_to_string() {
    let platform = Platform::linux_amd64();
    assert_eq!(platform.to_string(), "linux/amd64");

    let platform = Platform {
        os: "linux".to_string(),
        arch: "arm64".to_string(),
        variant: Some("v8".to_string()),
    };
    assert_eq!(platform.to_string(), "linux/arm64/v8");
}

#[test]
fn test_build_config_local_default() {
    let config = BuildConfig::local("./test");

    match config.source {
        DockerfileSource::Local { context_path, dockerfile_path } => {
            assert_eq!(context_path, PathBuf::from("./test"));
            assert_eq!(dockerfile_path, None);
        }
        _ => panic!("Expected Local source"),
    }

    assert_eq!(config.platforms.len(), 1);
    assert_eq!(config.platforms[0].to_string(), "linux/amd64");
    assert!(config.tags.is_empty());
    assert!(config.build_args.is_empty());
}

#[test]
fn test_build_config_github() {
    let config = BuildConfig::github("https://github.com/user/repo.git")
        .git_ref("main")
        .github_token("test_token");

    match config.source {
        DockerfileSource::GitHub { repo_url, git_ref, token, .. } => {
            assert_eq!(repo_url, "https://github.com/user/repo.git");
            assert_eq!(git_ref, Some("main".to_string()));
            assert_eq!(token, Some("test_token".to_string()));
        }
        _ => panic!("Expected GitHub source"),
    }
}

#[test]
fn test_build_config_builder_pattern() {
    let config = BuildConfig::local("./app")
        .dockerfile("custom.Dockerfile")
        .tag("myapp:v1")
        .tag("myapp:latest")
        .build_arg("VERSION", "1.0.0")
        .build_arg("ENV", "production")
        .target("production")
        .platform(Platform::linux_arm64())
        .no_cache(true)
        .pull(true);

    assert_eq!(config.tags.len(), 2);
    assert_eq!(config.tags[0], "myapp:v1");
    assert_eq!(config.tags[1], "myapp:latest");

    assert_eq!(config.build_args.len(), 2);
    assert_eq!(config.build_args.get("VERSION"), Some(&"1.0.0".to_string()));
    assert_eq!(config.build_args.get("ENV"), Some(&"production".to_string()));

    assert_eq!(config.target, Some("production".to_string()));
    assert_eq!(config.platforms.len(), 2); // default + added
    assert!(config.no_cache);
    assert!(config.pull);
}

#[test]
fn test_registry_auth() {
    let auth = RegistryAuth {
        host: "docker.io".to_string(),
        username: "testuser".to_string(),
        password: "testpass".to_string(),
    };

    let config = BuildConfig::local("./app")
        .registry_auth(auth);

    assert!(config.registry_auth.is_some());
    let registry_auth = config.registry_auth.unwrap();
    assert_eq!(registry_auth.host, "docker.io");
    assert_eq!(registry_auth.username, "testuser");
}

#[test]
fn test_cache_config() {
    let config = BuildConfig::local("./app")
        .cache_from("type=registry,ref=myapp:cache")
        .cache_to("type=inline");

    assert_eq!(config.cache_from.len(), 1);
    assert_eq!(config.cache_from[0], "type=registry,ref=myapp:cache");
    assert_eq!(config.cache_to.len(), 1);
    assert_eq!(config.cache_to[0], "type=inline");
}

#[test]
fn test_secrets_config() {
    let config = BuildConfig::local("./app")
        .secret("npm_token", "secret_value")
        .secret("api_key", "another_secret");

    assert_eq!(config.secrets.len(), 2);
    assert_eq!(config.secrets.get("npm_token"), Some(&"secret_value".to_string()));
    assert_eq!(config.secrets.get("api_key"), Some(&"another_secret".to_string()));
}

#[test]
fn test_multi_platform_build() {
    let config = BuildConfig::local("./app")
        .platform(Platform::linux_arm64())
        .platform(Platform::parse("linux/arm/v7").unwrap());

    assert_eq!(config.platforms.len(), 3); // default amd64 + 2 added
    assert_eq!(config.platforms[0].to_string(), "linux/amd64");
    assert_eq!(config.platforms[1].to_string(), "linux/arm64");
    assert_eq!(config.platforms[2].to_string(), "linux/arm/v7");
}

#[test]
fn test_dockerfile_path_local() {
    let config = BuildConfig::local("./app")
        .dockerfile("docker/Dockerfile.prod");

    match config.source {
        DockerfileSource::Local { dockerfile_path, .. } => {
            assert_eq!(dockerfile_path, Some(PathBuf::from("docker/Dockerfile.prod")));
        }
        _ => panic!("Expected Local source"),
    }
}

#[test]
fn test_dockerfile_path_github() {
    let config = BuildConfig::github("https://github.com/user/repo.git")
        .dockerfile("build/Dockerfile");

    match config.source {
        DockerfileSource::GitHub { dockerfile_path, .. } => {
            assert_eq!(dockerfile_path, Some("build/Dockerfile".to_string()));
        }
        _ => panic!("Expected GitHub source"),
    }
}
