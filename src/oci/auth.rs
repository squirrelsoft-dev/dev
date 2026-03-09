use oci_client::client::ClientConfig;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};

use crate::error::DevError;

/// Create an OCI client configured for anonymous access.
pub fn oci_client() -> Result<Client, DevError> {
    let config = ClientConfig {
        protocol: oci_client::client::ClientProtocol::Https,
        ..Default::default()
    };
    Client::try_from(config).map_err(|e| DevError::Registry(e.to_string()))
}

/// Returns the anonymous auth credential.
pub fn anonymous_auth() -> RegistryAuth {
    RegistryAuth::Anonymous
}

/// Authenticate anonymously against a registry for pull operations.
pub async fn auth_anonymous(client: &Client, reference: &Reference) -> Result<(), DevError> {
    client
        .auth(reference, &RegistryAuth::Anonymous, RegistryOperation::Pull)
        .await
        .map_err(|e| DevError::Registry(format!("authentication failed for {reference}: {e}")))?;
    Ok(())
}
