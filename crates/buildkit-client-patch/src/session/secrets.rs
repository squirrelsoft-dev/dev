//! Secrets protocol implementation for BuildKit sessions

use tonic::{Request, Response, Status};
use std::collections::HashMap;
use crate::proto::moby::secrets::v1::{
    secrets_server::Secrets,
    GetSecretRequest, GetSecretResponse,
};

/// Maximum secret size (500KB, matching BuildKit's MaxSecretSize)
const MAX_SECRET_SIZE: usize = 500 * 1024;

/// Secrets server implementation for BuildKit session
///
/// Provides secrets to BuildKit during build operations when using
/// `RUN --mount=type=secret,id=<secret_id>` in Dockerfiles.
#[derive(Debug, Clone, Default)]
pub struct SecretsServer {
    secrets: HashMap<String, Vec<u8>>,
}

impl SecretsServer {
    /// Create a new secrets server
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::SecretsServer;
    ///
    /// let secrets = SecretsServer::new();
    /// ```
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }

    /// Add a secret with the given ID and data
    ///
    /// # Arguments
    ///
    /// * `id` - Secret identifier (referenced in Dockerfile as `--mount=type=secret,id=<id>`)
    /// * `data` - Secret data as bytes
    ///
    /// # Returns
    ///
    /// Returns `Err` if the secret data exceeds MAX_SECRET_SIZE (500KB)
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::SecretsServer;
    ///
    /// let mut secrets = SecretsServer::new();
    /// secrets.add_secret("api_key", "secret_value".as_bytes().to_vec()).unwrap();
    /// ```
    pub fn add_secret(&mut self, id: impl Into<String>, data: Vec<u8>) -> Result<(), String> {
        if data.len() > MAX_SECRET_SIZE {
            return Err(format!("Secret size {} exceeds maximum of {}", data.len(), MAX_SECRET_SIZE));
        }
        self.secrets.insert(id.into(), data);
        Ok(())
    }

    /// Add a secret from a string value
    ///
    /// # Arguments
    ///
    /// * `id` - Secret identifier
    /// * `value` - Secret value as string
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::SecretsServer;
    ///
    /// let mut secrets = SecretsServer::new();
    /// secrets.add_secret_string("api_key", "secret_value").unwrap();
    /// ```
    pub fn add_secret_string(&mut self, id: impl Into<String>, value: impl AsRef<str>) -> Result<(), String> {
        self.add_secret(id, value.as_ref().as_bytes().to_vec())
    }

    /// Create a secrets server from a HashMap of string secrets
    ///
    /// # Arguments
    ///
    /// * `secrets` - HashMap mapping secret IDs to secret values
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use buildkit_client::session::SecretsServer;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("api_key".to_string(), "secret_value".to_string());
    /// let secrets = SecretsServer::from_map(map).unwrap();
    /// ```
    pub fn from_map(secrets: HashMap<String, String>) -> Result<Self, String> {
        let mut server = Self::new();
        for (id, value) in secrets {
            server.add_secret_string(id, value)?;
        }
        Ok(server)
    }
}

#[tonic::async_trait]
impl Secrets for SecretsServer {
    async fn get_secret(
        &self,
        request: Request<GetSecretRequest>,
    ) -> Result<Response<GetSecretResponse>, Status> {
        let req = request.into_inner();
        tracing::debug!("Secret requested - ID: {}, Annotations: {:?}", req.id, req.annotations);

        if let Some(data) = self.secrets.get(&req.id) {
            tracing::debug!("Found secret '{}' ({} bytes)", req.id, data.len());
            Ok(Response::new(GetSecretResponse {
                data: data.clone(),
            }))
        } else {
            tracing::warn!("Secret '{}' not found", req.id);
            Err(Status::not_found(format!("secret {} not found", req.id)))
        }
    }
}
