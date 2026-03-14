use std::io::Cursor;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::error::DevError;
use super::registry::pull_first_layer;

/// Return the blob cache directory, creating it if needed.
fn blob_cache_dir() -> Result<PathBuf, DevError> {
    let base = dirs::cache_dir()
        .ok_or_else(|| DevError::Cache("cannot determine cache directory".into()))?;
    let dir = base.join("dev").join("blobs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Compute the sha256 hex digest of bytes.
pub fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

/// Download an OCI artifact, cache the blob by digest, and extract to a temp directory.
/// Returns the path to the extracted directory.
pub async fn download_artifact(oci_ref: &str, tag: &str) -> Result<PathBuf, DevError> {
    let blob_dir = blob_cache_dir()?;
    let layer_bytes = pull_first_layer(oci_ref, tag).await?;
    let digest = sha256_hex(&layer_bytes);

    // Cache the raw blob
    let blob_path = blob_dir.join(&digest);
    if !blob_path.exists() {
        std::fs::write(&blob_path, &layer_bytes)?;
    }

    // Extract to a directory named by digest
    let extract_dir = blob_dir.join(format!("{digest}.d"));
    if !extract_dir.exists() {
        extract_archive(&layer_bytes, &extract_dir)?;
    }

    Ok(extract_dir)
}

/// Check if data starts with the gzip magic bytes (0x1f 0x8b).
fn is_gzip(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}

/// Extract an archive (tar.gz or plain tar) from bytes into the given directory.
pub fn extract_archive(data: &[u8], dest: &Path) -> Result<(), DevError> {
    std::fs::create_dir_all(dest)?;

    if is_gzip(data) {
        let decoder = GzDecoder::new(Cursor::new(data));
        let mut archive = Archive::new(decoder);
        archive.unpack(dest)?;
    } else {
        let mut archive = Archive::new(Cursor::new(data));
        archive.unpack(dest)?;
    }

    Ok(())
}
