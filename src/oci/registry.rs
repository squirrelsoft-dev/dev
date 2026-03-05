use oci_client::manifest::OciImageManifest;
use oci_client::{Client, Reference};

use crate::error::DevError;
use super::auth::{anonymous_auth, auth_anonymous, oci_client};

/// Pull an OCI image manifest and its digest.
pub async fn pull_manifest(
    client: &Client,
    reference: &Reference,
) -> Result<(OciImageManifest, String), DevError> {
    auth_anonymous(client, reference).await?;
    client
        .pull_image_manifest(reference, &anonymous_auth())
        .await
        .map_err(|e| DevError::Registry(format!("failed to pull manifest: {e}")))
}

/// Pull a blob from the registry into memory.
pub async fn pull_blob(
    client: &Client,
    reference: &Reference,
    digest: &str,
) -> Result<Vec<u8>, DevError> {
    auth_anonymous(client, reference).await?;
    let mut buf = Vec::new();
    client
        .pull_blob(reference, digest, &mut buf)
        .await
        .map_err(|e| DevError::Registry(format!("failed to pull blob: {e}")))?;
    Ok(buf)
}

/// Pull the first layer of an OCI artifact and return the raw bytes.
pub async fn pull_first_layer(oci_ref: &str, tag: &str) -> Result<Vec<u8>, DevError> {
    let reference: Reference = format!("{oci_ref}:{tag}")
        .parse()
        .map_err(|e: oci_client::ParseError| DevError::Registry(e.to_string()))?;

    let client = oci_client()?;
    let (manifest, _digest) = pull_manifest(&client, &reference).await?;

    let layer = manifest
        .layers
        .first()
        .ok_or_else(|| DevError::Registry("manifest has no layers".into()))?;

    pull_blob(&client, &reference, &layer.digest).await
}
