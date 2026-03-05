use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum DevError {
    #[error("No devcontainer configuration found in {0}")]
    NoConfig(String),

    #[error("Invalid devcontainer configuration: {0}")]
    InvalidConfig(String),

    #[error("Container runtime error: {0}")]
    Runtime(String),

    #[error("{0}")]
    NoRuntime(String),

    #[error("Container not found for workspace: {0}")]
    ContainerNotFound(String),

    #[error("OCI registry error: {0}")]
    Registry(String),

    #[error("Template not found: {0}")]
    TemplateNotFound(String),

    #[error("Feature not found: {0}")]
    FeatureNotFound(String),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Lifecycle hook failed: {command} (exit code {code})")]
    LifecycleHook { command: String, code: i32 },

    #[error("Image build failed: {0}")]
    BuildFailed(String),

    #[error("User cancelled operation")]
    Cancelled,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error(transparent)]
    Bollard(#[from] bollard::errors::Error),
}
