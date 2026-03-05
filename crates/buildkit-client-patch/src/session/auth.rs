//! Authentication protocol implementation for BuildKit sessions

use tonic::{Request, Response, Status};
use crate::proto::moby::filesync::v1::{
    auth_server::Auth,
    CredentialsRequest, CredentialsResponse,
    FetchTokenRequest, FetchTokenResponse,
    GetTokenAuthorityRequest, GetTokenAuthorityResponse,
    VerifyTokenAuthorityRequest, VerifyTokenAuthorityResponse,
};

/// Registry authentication configuration
///
/// Stores credentials for authenticating with container registries.
#[derive(Debug, Clone)]
pub struct RegistryAuthConfig {
    /// Registry hostname (e.g., "docker.io", "ghcr.io", "localhost:5000")
    pub host: String,
    /// Username for registry authentication
    pub username: String,
    /// Password or access token for registry authentication
    pub password: String,
}

/// Auth server implementation for BuildKit session
///
/// Handles registry authentication requests during image push operations.
#[derive(Debug, Clone, Default)]
pub struct AuthServer {
    registries: Vec<RegistryAuthConfig>,
}

impl AuthServer {
    /// Create a new authentication server
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::AuthServer;
    ///
    /// let auth = AuthServer::new();
    /// ```
    pub fn new() -> Self {
        Self {
            registries: Vec::new(),
        }
    }

    /// Add registry credentials
    ///
    /// # Arguments
    ///
    /// * `config` - Registry authentication configuration
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::{AuthServer, RegistryAuthConfig};
    ///
    /// let mut auth = AuthServer::new();
    /// auth.add_registry(RegistryAuthConfig {
    ///     host: "docker.io".to_string(),
    ///     username: "myuser".to_string(),
    ///     password: "mytoken".to_string(),
    /// });
    /// ```
    pub fn add_registry(&mut self, config: RegistryAuthConfig) {
        self.registries.push(config);
    }

    fn find_credentials(&self, host: &str) -> Option<&RegistryAuthConfig> {
        self.registries.iter().find(|r| {
            r.host == host ||
            host.contains(&r.host) ||
            // Handle docker.io specially
            (r.host == "docker.io" && (host == "registry-1.docker.io" || host == "index.docker.io"))
        })
    }
}

#[tonic::async_trait]
impl Auth for AuthServer {
    async fn credentials(
        &self,
        request: Request<CredentialsRequest>,
    ) -> Result<Response<CredentialsResponse>, Status> {
        let req = request.into_inner();
        tracing::debug!("Credentials requested for host: {}", req.host);

        if let Some(config) = self.find_credentials(&req.host) {
            tracing::debug!("Found credentials for host: {}", req.host);
            Ok(Response::new(CredentialsResponse {
                username: config.username.clone(),
                secret: config.password.clone(),
            }))
        } else {
            tracing::debug!("No credentials found for host: {}", req.host);
            // Return empty credentials (anonymous access)
            Ok(Response::new(CredentialsResponse {
                username: String::new(),
                secret: String::new(),
            }))
        }
    }

    async fn fetch_token(
        &self,
        request: Request<FetchTokenRequest>,
    ) -> Result<Response<FetchTokenResponse>, Status> {
        let req = request.into_inner();
        tracing::debug!(
            "FetchToken requested - Host: {}, Realm: {}, Service: {}, Scopes: {:?}",
            req.host, req.realm, req.service, req.scopes
        );

        // For most cases, BuildKit will handle token exchange
        // We just need to provide basic auth credentials via the Credentials RPC
        Ok(Response::new(FetchTokenResponse {
            token: String::new(),
            expires_in: 0,
            issued_at: 0,
        }))
    }

    async fn get_token_authority(
        &self,
        _request: Request<GetTokenAuthorityRequest>,
    ) -> Result<Response<GetTokenAuthorityResponse>, Status> {
        // Not implementing token authority for now
        Ok(Response::new(GetTokenAuthorityResponse {
            public_key: vec![],
        }))
    }

    async fn verify_token_authority(
        &self,
        _request: Request<VerifyTokenAuthorityRequest>,
    ) -> Result<Response<VerifyTokenAuthorityResponse>, Status> {
        // Not implementing token authority for now
        Ok(Response::new(VerifyTokenAuthorityResponse {
            signed: vec![],
        }))
    }
}
