//! Read access to the Apple Containers on-disk content store.
//!
//! When a build's base image is not already cached inside the builder VM, the
//! shim asks the host for the image's blobs over the `content-store` stage
//! (`pkg/content/info.go`, `pkg/content/readerat.go`). The daemon keeps those
//! blobs in a content-addressed layout that we can serve directly.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::error::AppleContainerError;

/// Root of the daemon's application-support directory, or `None` when `HOME`
/// does not locate one.
///
/// `HOME` is the only thing that names this directory, and everything under it
/// is trusted: the blobs an image's environment and working directory are read
/// out of, and the archives `imageLoad` registers images from. Guessing a
/// world-writable fallback such as `/tmp` when `HOME` is unset or relative
/// would let any other local user pre-populate both, so an unusable `HOME` is
/// reported rather than substituted.
pub fn application_support_root() -> Option<PathBuf> {
    support_root_under(std::env::var_os("HOME").as_deref())
}

/// [`application_support_root`] against an explicit home directory.
fn support_root_under(home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let home = PathBuf::from(home?);
    if !home.is_absolute() {
        return None;
    }
    Some(home.join("Library/Application Support/com.apple.container"))
}

/// Locate a blob by its OCI digest.
///
/// Returns `None` for a digest we cannot map to a file, including malformed
/// ones — the components are checked so a digest can never escape the store.
pub fn blob_path(digest: &str) -> Option<PathBuf> {
    let (algorithm, hex) = digest.split_once(':')?;
    if !is_safe_component(algorithm) || !is_safe_component(hex) {
        return None;
    }
    Some(
        application_support_root()?
            .join("content/blobs")
            .join(algorithm)
            .join(hex),
    )
}

/// Reject anything that could traverse out of the content store.
///
/// The separators an OCI digest may contain are allowed, but a component made
/// only of them — `.` or `..` — names a directory rather than a blob and would
/// walk back up the tree.
fn is_safe_component(component: &str) -> bool {
    component.chars().any(|c| c.is_ascii_alphanumeric())
        && component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '+')
}

/// Size of a locally stored blob, or `None` when it is not present.
pub fn blob_size(digest: &str) -> Option<u64> {
    let path = blob_path(digest)?;
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Read a byte range out of a locally stored blob.
///
/// A range past the end yields an empty buffer, which the shim's `ReadAt`
/// reads as EOF.
pub fn read_blob_range(
    digest: &str,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, AppleContainerError> {
    let path = blob_path(digest).ok_or_else(|| {
        AppleContainerError::XpcError(format!("content store: malformed digest {digest:?}"))
    })?;
    read_range(&path, offset, length)
}

/// Read a byte range out of a file, clamped to its end.
///
/// Both the content store and the fssync `Read` handler answer
/// offset/length requests whose final call always overruns the file, and both
/// protocols read an empty result as EOF.
pub fn read_range(
    path: &std::path::Path,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, AppleContainerError> {
    let mut file = std::fs::File::open(path).map_err(AppleContainerError::Io)?;
    let remaining = file
        .metadata()
        .map_err(AppleContainerError::Io)?
        .len()
        .saturating_sub(offset);
    let wanted = (length as u64).min(remaining) as usize;
    if wanted == 0 {
        return Ok(Vec::new());
    }

    file.seek(SeekFrom::Start(offset))
        .map_err(AppleContainerError::Io)?;
    let mut buffer = vec![0u8; wanted];
    file.read_exact(&mut buffer)
        .map_err(AppleContainerError::Io)?;
    Ok(buffer)
}

/// Parse a JSON blob out of the content store.
pub fn read_json_blob(digest: &str) -> Option<serde_json::Value> {
    let path = blob_path(digest)?;
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Read an image's OCI config, starting from its root descriptor digest.
///
/// A locally built image has no registry to query, so its config — the
/// environment, working directory and user the container inherits — can only
/// come from the store the daemon just loaded it into. The root descriptor is
/// either a manifest or an index; an index is narrowed to the entry matching
/// the requested platform first.
pub fn read_image_config(
    root_digest: &str,
    os: &str,
    architecture: &str,
) -> Option<serde_json::Value> {
    // An index may point at another index; the nesting is bounded so a
    // malformed or self-referential blob cannot loop forever.
    let mut digest = root_digest.to_string();
    for _ in 0..4 {
        let blob = read_json_blob(&digest)?;
        match blob.get("manifests").and_then(|m| m.as_array()) {
            Some(entries) => digest = select_platform_manifest(entries, os, architecture)?,
            None => {
                let config = blob.get("config")?.get("digest")?.as_str()?;
                return read_json_blob(config);
            }
        }
    }
    None
}

/// Pick the index entry built for a platform.
///
/// Registries attach attestation entries carrying an `unknown/unknown`
/// platform, so an exact match is required rather than a first-entry guess.
fn select_platform_manifest(
    entries: &[serde_json::Value],
    os: &str,
    architecture: &str,
) -> Option<String> {
    entries
        .iter()
        .find(|entry| {
            let platform = entry.get("platform");
            let entry_os = platform.and_then(|p| p.get("os")).and_then(|v| v.as_str());
            let entry_arch = platform
                .and_then(|p| p.get("architecture"))
                .and_then(|v| v.as_str());
            entry_os == Some(os) && entry_arch == Some(architecture)
        })
        .and_then(|entry| entry.get("digest"))
        .and_then(|d| d.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_maps_into_the_content_addressed_layout() {
        let path = blob_path("sha256:abc123").expect("well-formed digest");
        assert!(path.ends_with("content/blobs/sha256/abc123"), "{path:?}");
    }

    /// Everything under this root is trusted — the image config a container's
    /// environment comes from, the archives images are loaded out of — so a
    /// `HOME` that does not name a private directory must yield no root at all
    /// rather than a world-writable guess another local user can pre-populate.
    #[test]
    fn an_unusable_home_yields_no_content_store_root() {
        use std::ffi::OsStr;

        assert!(support_root_under(Some(OsStr::new("/Users/dev"))).is_some());
        assert_eq!(support_root_under(None), None);
        assert_eq!(support_root_under(Some(OsStr::new(""))), None);
        assert_eq!(support_root_under(Some(OsStr::new("relative/home"))), None);
    }

    /// A digest arrives from the builder, so it must never be able to name a
    /// file outside the content store — a `ReaderAt` reply would then hand the
    /// builder the contents of an arbitrary host file.
    #[test]
    fn traversal_and_malformed_digests_are_refused() {
        assert_eq!(blob_path("sha256:../../../../etc/passwd"), None);
        assert_eq!(blob_path("../etc:passwd"), None);
        assert_eq!(blob_path("sha256:"), None);
        assert_eq!(blob_path(":abc"), None);
        assert_eq!(blob_path("no-separator"), None);
        assert_eq!(blob_path("sha256:has/slash"), None);
        // A component of only dots names a directory, not a blob.
        assert_eq!(blob_path("sha256:.."), None);
        assert_eq!(blob_path("sha256:."), None);
        assert_eq!(blob_path("..:.."), None);
    }

    /// The OCI grammar lets an algorithm carry `+._-` separators.
    #[test]
    fn digests_using_the_full_oci_algorithm_grammar_are_accepted() {
        assert!(blob_path("sha256:abc123").is_some());
        assert!(blob_path("sha512+b64:abc123").is_some());
        assert!(blob_path("multihash.sha2-256:abc123").is_some());
    }

    #[test]
    fn a_missing_blob_reports_no_size_rather_than_zero() {
        assert_eq!(blob_size("sha256:definitelynotpresent0000000"), None);
        assert_eq!(blob_size("sha256:../escape"), None);
    }

    /// The shim's prefetcher issues fixed-size `ReadAt` calls, so the final
    /// one always runs past the end of the blob; it reads an empty reply as
    /// EOF and an error as a failed build.
    #[test]
    fn ranges_are_clamped_to_the_end_of_the_blob() {
        let dir = tempfile::tempdir().expect("temp store");
        let blob = dir.path().join("blob");
        std::fs::write(&blob, b"0123456789").expect("blob");

        assert_eq!(read_range(&blob, 0, 4).expect("head"), b"0123");
        assert_eq!(read_range(&blob, 8, 100).expect("clamped tail"), b"89");
        assert_eq!(read_range(&blob, 0, 10).expect("whole blob"), b"0123456789");
        assert!(
            read_range(&blob, 10, 10).expect("at end").is_empty(),
            "reading at the end must read as EOF, not error"
        );
        assert!(
            read_range(&blob, 100, 10).expect("past end").is_empty(),
            "reading past the end must read as EOF, not error"
        );
        // `ReaderAt::init` probes with offset 0 and length 0 to learn the size.
        assert!(read_range(&blob, 0, 0).expect("size probe").is_empty());
    }

    #[test]
    fn reading_a_blob_that_is_not_stored_locally_fails_loudly() {
        let dir = tempfile::tempdir().expect("temp store");
        assert!(read_range(&dir.path().join("absent"), 0, 4).is_err());
    }

    fn index_entries() -> Vec<serde_json::Value> {
        serde_json::from_str(
            r#"[
                {"digest": "sha256:amd64manifest",
                 "platform": {"os": "linux", "architecture": "amd64"}},
                {"digest": "sha256:arm64manifest",
                 "platform": {"os": "linux", "architecture": "arm64"}},
                {"digest": "sha256:attestation",
                 "platform": {"os": "unknown", "architecture": "unknown"}}
            ]"#,
        )
        .expect("index fixture")
    }

    #[test]
    fn an_index_narrows_to_the_matching_platform() {
        let entries = index_entries();
        assert_eq!(
            select_platform_manifest(&entries, "linux", "arm64").as_deref(),
            Some("sha256:arm64manifest")
        );
        assert_eq!(
            select_platform_manifest(&entries, "linux", "amd64").as_deref(),
            Some("sha256:amd64manifest")
        );
    }

    /// Guessing the first entry would pick an attestation manifest, which
    /// carries no runnable image config.
    #[test]
    fn an_index_without_a_matching_platform_selects_nothing() {
        let entries = index_entries();
        assert_eq!(select_platform_manifest(&entries, "linux", "s390x"), None);
        assert_eq!(select_platform_manifest(&entries, "darwin", "arm64"), None);
        assert_eq!(select_platform_manifest(&[], "linux", "arm64"), None);
    }

    #[test]
    fn entries_without_platform_metadata_are_skipped() {
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"digest": "sha256:bare"}]"#).expect("fixture");
        assert_eq!(select_platform_manifest(&entries, "linux", "arm64"), None);
    }
}
