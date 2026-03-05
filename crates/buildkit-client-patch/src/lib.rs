//! BuildKit Rust Client
//!
//! A Rust client library for interacting with BuildKit to build container images.
//!
//! # Features
//!
//! - Build from local Dockerfile or GitHub repository
//! - Support for private GitHub repositories with authentication
//! - Push images to registries with authentication
//! - Multi-platform builds
//! - Build arguments, target stages, and advanced options
//! - Real-time progress monitoring
//! - Cache import/export
//!
//! # Examples
//!
//! ## Build from local Dockerfile
//!
//! ```no_run
//! use buildkit_client::{BuildKitClient, BuildConfig, RegistryAuth};
//! use buildkit_client::progress::ConsoleProgressHandler;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut client = BuildKitClient::connect("http://localhost:1234").await?;
//!
//!     let config = BuildConfig::local("./my-app")
//!         .tag("localhost:5000/my-app:latest")
//!         .build_arg("VERSION", "1.0.0");
//!
//!     let progress = Box::new(ConsoleProgressHandler::new(true));
//!     let result = client.build(config, Some(progress)).await?;
//!
//!     println!("Image digest: {:?}", result.digest);
//!     Ok(())
//! }
//! ```
//!
//! ## Build from GitHub repository
//!
//! ```no_run
//! use buildkit_client::{BuildKitClient, BuildConfig};
//! use buildkit_client::progress::ConsoleProgressHandler;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut client = BuildKitClient::connect("http://localhost:1234").await?;
//!
//!     let config = BuildConfig::github("https://github.com/user/repo.git")
//!         .git_ref("main")
//!         .github_token("ghp_your_token_here")
//!         .tag("localhost:5000/my-app:latest");
//!
//!     let progress = Box::new(ConsoleProgressHandler::new(true));
//!     let result = client.build(config, Some(progress)).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Multi-platform build with registry authentication
//!
//! ```no_run
//! use buildkit_client::{BuildKitClient, BuildConfig, Platform, RegistryAuth};
//! use buildkit_client::progress::ConsoleProgressHandler;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut client = BuildKitClient::connect("http://localhost:1234").await?;
//!
//!     let config = BuildConfig::local(".")
//!         .platform(Platform::linux_amd64())
//!         .platform(Platform::linux_arm64())
//!         .tag("docker.io/myuser/myapp:latest")
//!         .registry_auth(RegistryAuth {
//!             host: "docker.io".to_string(),
//!             username: "myuser".to_string(),
//!             password: "mytoken".to_string(),
//!         });
//!
//!     let progress = Box::new(ConsoleProgressHandler::new(true));
//!     let result = client.build(config, Some(progress)).await?;
//!
//!     println!("Multi-platform image built: {:?}", result.digest);
//!     Ok(())
//! }
//! ```

pub mod proto;
pub mod error;
pub mod builder;
pub mod client;
pub mod progress;
pub mod solve;
pub mod session;

// Re-export main types
pub use builder::{BuildConfig, DockerfileSource, Platform, RegistryAuth};
pub use client::BuildKitClient;
pub use solve::BuildResult;
pub use error::{Error, Result};
