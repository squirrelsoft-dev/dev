use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppleContainerError {
    #[error("XPC connection failed: {0}")]
    ConnectionFailed(String),

    #[error("XPC send failed: {0}")]
    SendFailed(String),

    #[error("XPC error reply: {0}")]
    XpcError(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Container not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
