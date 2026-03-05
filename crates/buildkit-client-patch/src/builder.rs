//! Build operations and configuration

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Source location for Dockerfile
#[derive(Debug, Clone)]
pub enum DockerfileSource {
    /// Local filesystem path
    Local {
        /// Path to the context directory
        context_path: PathBuf,
        /// Path to the Dockerfile (relative to context or absolute)
        dockerfile_path: Option<PathBuf>,
    },
    /// GitHub repository
    GitHub {
        /// Repository URL (e.g., "<https://github.com/user/repo.git>")
        repo_url: String,
        /// Git reference (branch, tag, or commit SHA)
        git_ref: Option<String>,
        /// Path to Dockerfile within the repository
        dockerfile_path: Option<String>,
        /// GitHub token for private repositories
        token: Option<String>,
    },
}

/// Platform specification for multi-platform builds
#[derive(Debug, Clone)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub variant: Option<String>,
}

impl Platform {
    /// Create a Linux AMD64 platform
    pub fn linux_amd64() -> Self {
        Self {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            variant: None,
        }
    }

    /// Create a Linux ARM64 platform
    pub fn linux_arm64() -> Self {
        Self {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
            variant: None,
        }
    }

    /// Parse platform from string (e.g., "linux/amd64", "linux/arm64/v8")
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        match parts.as_slice() {
            [os, arch] => Ok(Self {
                os: os.to_string(),
                arch: arch.to_string(),
                variant: None,
            }),
            [os, arch, variant] => Ok(Self {
                os: os.to_string(),
                arch: arch.to_string(),
                variant: Some(variant.to_string()),
            }),
            _ => Err(Error::InvalidPlatform(s.to_string())),
        }
    }

}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(variant) = &self.variant {
            write!(f, "{}/{}/{}", self.os, self.arch, variant)
        } else {
            write!(f, "{}/{}", self.os, self.arch)
        }
    }
}

/// Registry authentication credentials
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    /// Registry host (e.g., "docker.io", "localhost:5000")
    pub host: String,
    /// Username
    pub username: String,
    /// Password or token
    pub password: String,
}

/// Build configuration
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Dockerfile source
    pub source: DockerfileSource,

    /// Build arguments (ARG values)
    pub build_args: HashMap<String, String>,

    /// Target stage in multi-stage build
    pub target: Option<String>,

    /// Target platforms
    pub platforms: Vec<Platform>,

    /// Image tags to push
    pub tags: Vec<String>,

    /// Registry authentication
    pub registry_auth: Option<RegistryAuth>,

    /// Cache imports (registry or local paths)
    pub cache_from: Vec<String>,

    /// Cache exports
    pub cache_to: Vec<String>,

    /// Secrets to mount during build
    pub secrets: HashMap<String, String>,

    /// SSH agent sockets to forward
    pub ssh_agents: Vec<String>,

    /// No cache flag
    pub no_cache: bool,

    /// Pull always flag
    pub pull: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            source: DockerfileSource::Local {
                context_path: PathBuf::from("."),
                dockerfile_path: None,
            },
            build_args: HashMap::new(),
            target: None,
            platforms: vec![Platform::linux_amd64()],
            tags: Vec::new(),
            registry_auth: None,
            cache_from: Vec::new(),
            cache_to: Vec::new(),
            secrets: HashMap::new(),
            ssh_agents: Vec::new(),
            no_cache: false,
            pull: false,
        }
    }
}

impl BuildConfig {
    /// Create a new build configuration with local Dockerfile
    pub fn local(context_path: impl Into<PathBuf>) -> Self {
        Self {
            source: DockerfileSource::Local {
                context_path: context_path.into(),
                dockerfile_path: None,
            },
            ..Default::default()
        }
    }

    /// Create a new build configuration with GitHub repository
    pub fn github(repo_url: impl Into<String>) -> Self {
        Self {
            source: DockerfileSource::GitHub {
                repo_url: repo_url.into(),
                git_ref: None,
                dockerfile_path: None,
                token: None,
            },
            ..Default::default()
        }
    }

    /// Set Dockerfile path
    pub fn dockerfile(mut self, path: impl Into<String>) -> Self {
        match &mut self.source {
            DockerfileSource::Local { dockerfile_path, .. } => {
                *dockerfile_path = Some(PathBuf::from(path.into()));
            }
            DockerfileSource::GitHub { dockerfile_path, .. } => {
                *dockerfile_path = Some(path.into());
            }
        }
        self
    }

    /// Add a build argument
    pub fn build_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.build_args.insert(key.into(), value.into());
        self
    }

    /// Set target stage
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Add a platform
    pub fn platform(mut self, platform: Platform) -> Self {
        self.platforms.push(platform);
        self
    }

    /// Add an image tag
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Set registry authentication
    pub fn registry_auth(mut self, auth: RegistryAuth) -> Self {
        self.registry_auth = Some(auth);
        self
    }

    /// Set GitHub token for private repositories
    pub fn github_token(mut self, token: impl Into<String>) -> Self {
        if let DockerfileSource::GitHub { token: ref mut t, .. } = &mut self.source {
            *t = Some(token.into());
        }
        self
    }

    /// Set git reference (branch, tag, or commit)
    pub fn git_ref(mut self, git_ref: impl Into<String>) -> Self {
        if let DockerfileSource::GitHub { git_ref: ref mut r, .. } = &mut self.source {
            *r = Some(git_ref.into());
        }
        self
    }

    /// Add cache import source
    pub fn cache_from(mut self, source: impl Into<String>) -> Self {
        self.cache_from.push(source.into());
        self
    }

    /// Add cache export destination
    pub fn cache_to(mut self, dest: impl Into<String>) -> Self {
        self.cache_to.push(dest.into());
        self
    }

    /// Add a secret
    pub fn secret(mut self, id: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(id.into(), value.into());
        self
    }

    /// Set no-cache flag
    pub fn no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// Set pull flag
    pub fn pull(mut self, pull: bool) -> Self {
        self.pull = pull;
        self
    }
}
