//! BuildKit session implementation for file access and streaming

pub mod filesync;
pub mod auth;
pub mod secrets;
pub mod grpc_tunnel;

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::transport::Channel;
use uuid::Uuid;

use crate::proto::moby::buildkit::v1::{BytesMessage, control_client::ControlClient};
use grpc_tunnel::GrpcTunnel;

pub use filesync::FileSyncServer;
pub use auth::{AuthServer, RegistryAuthConfig};
pub use secrets::SecretsServer;

/// Session manager for BuildKit
///
/// Manages a BuildKit session lifecycle including file synchronization,
/// authentication, and bidirectional gRPC streaming.
pub struct Session {
    /// Unique session identifier (UUID format)
    pub id: String,
    /// Shared key for session identification in BuildKit requests
    pub shared_key: String,
    tx: Option<mpsc::Sender<BytesMessage>>,
    services: Arc<Mutex<SessionServices>>,
}

/// Session service handlers
struct SessionServices {
    file_sync: Option<FileSyncServer>,
    auth: Option<AuthServer>,
    secrets: Option<SecretsServer>,
}

impl Session {
    /// Create a new session
    pub fn new() -> Self {
        let id = Uuid::new_v4().to_string();
        let shared_key = format!("session-{}", Uuid::new_v4());

        Self {
            id,
            shared_key,
            tx: None,
            services: Arc::new(Mutex::new(SessionServices {
                file_sync: None,
                auth: None,
                secrets: None,
            })),
        }
    }

    /// Add file sync service for a specific directory
    pub async fn add_file_sync(&mut self, root_path: PathBuf) {
        let mut services = self.services.lock().await;
        services.file_sync = Some(FileSyncServer::new(root_path));
        tracing::debug!("Added FileSync service");
    }

    /// Add authentication service
    pub async fn add_auth(&mut self, auth: AuthServer) {
        let mut services = self.services.lock().await;
        services.auth = Some(auth);
        tracing::debug!("Added Auth service");
    }

    /// Add secrets service
    pub async fn add_secrets(&mut self, secrets: SecretsServer) {
        let mut services = self.services.lock().await;
        services.secrets = Some(secrets);
        tracing::debug!("Added Secrets service");
    }

    /// Start a session with BuildKit
    pub async fn start(&mut self, mut control: ControlClient<Channel>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<BytesMessage>(128);
        let session_id = self.id.clone();
        let services = Arc::clone(&self.services);

        tracing::info!("Starting session: {}", session_id);

        // Create the outbound stream
        let outbound = async_stream::stream! {
            while let Some(msg) = rx.recv().await {
                yield msg;
            }
        };

        // Create request with session metadata headers
        let mut request = tonic::Request::new(outbound);
        let metadata = request.metadata_mut();

        // Add session metadata headers
        for (key, values) in self.metadata() {
            if let Ok(k) = key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>() {
                for value in values {
                    if let Ok(v) = value.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                        metadata.append(k.clone(), v);
                    }
                }
            }
        }

        // Start the session
        let response = control
            .session(request)
            .await?;

        let mut inbound = response.into_inner();

        // Create channels for the HTTP/2 tunnel
        let (inbound_tx, inbound_rx) = mpsc::channel::<BytesMessage>(128);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<BytesMessage>(128);

        // Get services for tunnel
        let services_guard = services.lock().await;
        let file_sync = services_guard.file_sync.clone();
        let auth = services_guard.auth.clone();
        let secrets = services_guard.secrets.clone();
        drop(services_guard);

        // Spawn task to receive from BuildKit and forward to tunnel
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if let Err(e) = inbound_tx.send(msg).await {
                    tracing::error!("Failed to forward inbound message: {}", e);
                    break;
                }
            }
            tracing::info!("Session {} inbound ended", session_id);
        });

        // Spawn task to receive from tunnel and forward to BuildKit
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if let Err(e) = tx_clone.send(msg).await {
                    tracing::error!("Failed to forward outbound message: {}", e);
                    break;
                }
            }
        });

        // Start the HTTP/2 server in the tunnel
        let tunnel = GrpcTunnel::new(tx.clone(), file_sync, auth, secrets);
        tokio::spawn(async move {
            if let Err(e) = tunnel.serve(inbound_rx, outbound_tx).await {
                tracing::error!("HTTP/2 tunnel error: {}", e);
            }
        });

        self.tx = Some(tx);
        Ok(())
    }

    /// Get session metadata to attach to solve request
    pub fn metadata(&self) -> HashMap<String, Vec<String>> {
        let mut meta = HashMap::new();
        meta.insert("X-Docker-Expose-Session-Uuid".to_string(), vec![self.id.clone()]);
        meta.insert("X-Docker-Expose-Session-Name".to_string(), vec![self.shared_key.clone()]);
        meta.insert("X-Docker-Expose-Session-Sharedkey".to_string(), vec![self.shared_key.clone()]);

        // Add supported gRPC methods
        let methods = vec![
            "/grpc.health.v1.Health/Check".to_string(),
            "/moby.filesync.v1.FileSync/DiffCopy".to_string(),
            "/moby.filesync.v1.FileSync/TarStream".to_string(),
            "/moby.filesync.v1.Auth/Credentials".to_string(),
            "/moby.filesync.v1.Auth/FetchToken".to_string(),
            "/moby.filesync.v1.Auth/GetTokenAuthority".to_string(),
            "/moby.filesync.v1.Auth/VerifyTokenAuthority".to_string(),
            "/moby.buildkit.secrets.v1.Secrets/GetSecret".to_string(),
        ];
        meta.insert("X-Docker-Expose-Session-Grpc-Method".to_string(), methods);

        meta
    }

    /// Send a message to the session stream
    pub async fn send(&self, msg: BytesMessage) -> Result<()> {
        if let Some(ref tx) = self.tx {
            tx.send(msg)
                .await
                .map_err(|_| Error::send_failed("BytesMessage", "channel closed"))?;
            Ok(())
        } else {
            Err(Error::SessionNotStarted)
        }
    }

    /// Get session ID for SolveRequest
    pub fn get_id(&self) -> String {
        self.id.clone()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// File sync helper for sending local files to BuildKit
pub struct FileSync {
    context_path: PathBuf,
}

impl FileSync {
    /// Create a new FileSync helper with the given context path
    ///
    /// # Arguments
    ///
    /// * `context_path` - Path to the build context directory
    pub fn new(context_path: impl Into<PathBuf>) -> Self {
        Self {
            context_path: context_path.into(),
        }
    }

    /// Check if path exists and is accessible
    pub fn validate(&self) -> Result<()> {
        if !self.context_path.exists() {
            return Err(Error::PathNotFound(self.context_path.clone()));
        }
        if !self.context_path.is_dir() {
            return Err(Error::NotADirectory(self.context_path.clone()));
        }
        Ok(())
    }

    /// Get absolute path
    pub fn absolute_path(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.context_path)
            .map_err(|e| Error::PathResolution {
                path: self.context_path.clone(),
                source: e,
            })
    }
}
