//! Error types for BuildKit client operations

use std::path::PathBuf;
use thiserror::Error;

/// Result type alias for BuildKit operations
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for BuildKit client operations
#[derive(Error, Debug)]
pub enum Error {
    /// Connection-related errors
    #[error("Failed to connect to BuildKit at {endpoint}: {source}")]
    Connection {
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },

    /// Invalid endpoint URL
    #[error("Invalid BuildKit endpoint URL: {0}")]
    InvalidEndpoint(String),

    /// gRPC communication errors
    #[error("gRPC communication failed: {0}")]
    Grpc(Box<tonic::Status>),

    /// Session-related errors
    #[error("Session error: {0}")]
    Session(String),

    /// Session not started
    #[error("Session has not been started")]
    SessionNotStarted,

    /// File system errors
    #[error("File system error: {0}")]
    Io(#[from] std::io::Error),

    /// Path does not exist
    #[error("Path does not exist: {0}")]
    PathNotFound(PathBuf),

    /// Path is not a directory
    #[error("Path is not a directory: {0}")]
    NotADirectory(PathBuf),

    /// Path is outside root directory
    #[error("Path {path} is outside root directory")]
    PathOutsideRoot { path: String },

    /// Failed to resolve absolute path
    #[error("Failed to resolve absolute path for {path}: {source}")]
    PathResolution {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Build execution errors
    #[error("Build execution failed: {0}")]
    Build(String),

    /// Invalid build configuration
    #[error("Invalid build configuration: {0}")]
    InvalidConfig(String),

    /// Invalid platform format
    #[error("Invalid platform format: {0}")]
    InvalidPlatform(String),

    /// Progress monitoring errors
    #[error("Progress monitoring failed: {0}")]
    Progress(String),

    /// Protocol errors
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// HTTP/2 handshake failed
    #[error("HTTP/2 handshake failed: {source}")]
    Http2Handshake {
        #[source]
        source: h2::Error,
    },

    /// HTTP/2 stream error
    #[error("HTTP/2 stream error: {source}")]
    Http2Stream {
        #[source]
        source: h2::Error,
    },

    /// Failed to send message
    #[error("Failed to send {message_type}: {reason}")]
    SendFailed {
        message_type: String,
        reason: String,
    },

    /// Decoding error
    #[error("Failed to decode {message_type}: {source}")]
    Decode {
        message_type: String,
        #[source]
        source: prost::DecodeError,
    },

    /// Encoding error
    #[error("Failed to encode {message_type}: {source}")]
    Encode {
        message_type: String,
        #[source]
        source: prost::EncodeError,
    },

    /// Secrets error
    #[error("Secrets error: {0}")]
    Secrets(String),

    /// Secret not found
    #[error("Secret not found: {0}")]
    SecretNotFound(String),

    /// Secrets service not configured
    #[error("Secrets service is not configured")]
    SecretsNotConfigured,

    /// Generic error for compatibility during migration
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Create a session error
    pub fn session(msg: impl Into<String>) -> Self {
        Error::Session(msg.into())
    }

    /// Create a build error
    pub fn build(msg: impl Into<String>) -> Self {
        Error::Build(msg.into())
    }

    /// Create a protocol error
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// Create a progress error
    pub fn progress(msg: impl Into<String>) -> Self {
        Error::Progress(msg.into())
    }

    /// Create a secrets error
    pub fn secrets(msg: impl Into<String>) -> Self {
        Error::Secrets(msg.into())
    }

    /// Create a send failed error
    pub fn send_failed(message_type: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::SendFailed {
            message_type: message_type.into(),
            reason: reason.into(),
        }
    }

    /// Create a decode error
    pub fn decode(message_type: impl Into<String>, source: prost::DecodeError) -> Self {
        Error::Decode {
            message_type: message_type.into(),
            source,
        }
    }

    /// Create an encode error
    pub fn encode(message_type: impl Into<String>, source: prost::EncodeError) -> Self {
        Error::Encode {
            message_type: message_type.into(),
            source,
        }
    }

    /// Create an other error for compatibility
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

// Implement From for common error types
impl From<prost::EncodeError> for Error {
    fn from(e: prost::EncodeError) -> Self {
        Error::Encode {
            message_type: "unknown".to_string(),
            source: e,
        }
    }
}

impl From<prost::DecodeError> for Error {
    fn from(e: prost::DecodeError) -> Self {
        Error::Decode {
            message_type: "unknown".to_string(),
            source: e,
        }
    }
}

impl From<tonic::Status> for Error {
    fn from(status: tonic::Status) -> Self {
        Error::Grpc(Box::new(status))
    }
}
