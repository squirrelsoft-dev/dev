//! BuildKit gRPC client implementation

use crate::error::{Error, Result};
use crate::proto::moby::buildkit::v1::control_client::ControlClient;
use tonic::transport::{Channel, Endpoint};

/// BuildKit client for interacting with buildkitd
#[derive(Clone)]
pub struct BuildKitClient {
    control: ControlClient<Channel>,
}

impl BuildKitClient {
    /// Create a new BuildKit client connected to the specified address
    ///
    /// # Arguments
    /// * `addr` - The address of the buildkitd service (e.g., "http://localhost:1234")
    ///
    /// # Example
    /// ```no_run
    /// use buildkit_client::client::BuildKitClient;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let client = BuildKitClient::connect("http://localhost:1234").await?;
    ///     Ok(())
    /// }
    /// ```
    pub async fn connect(addr: impl Into<String>) -> Result<Self> {
        let addr = addr.into();
        tracing::info!("Connecting to buildkitd at {}", addr);

        let endpoint = Endpoint::from_shared(addr.clone())
            .map_err(|_| Error::InvalidEndpoint(addr.clone()))?
            .timeout(std::time::Duration::from_secs(30));

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| Error::Connection {
                endpoint: addr,
                source: e,
            })?;

        let control = ControlClient::new(channel);

        tracing::info!("Successfully connected to buildkitd");

        Ok(Self { control })
    }

    /// Create a BuildKit client from an existing tonic Channel.
    pub fn from_channel(channel: Channel) -> Self {
        let control = ControlClient::new(channel);
        Self { control }
    }

    /// Get a reference to the control client
    pub fn control(&mut self) -> &mut ControlClient<Channel> {
        &mut self.control
    }

    /// Check if the buildkitd service is available
    pub async fn health_check(&mut self) -> Result<()> {
        use crate::proto::moby::buildkit::v1::InfoRequest;

        let _info = self
            .control
            .info(InfoRequest {})
            .await?;

        tracing::debug!("BuildKit health check passed");
        Ok(())
    }
}
