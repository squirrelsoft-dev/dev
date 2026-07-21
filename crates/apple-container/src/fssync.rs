//! Client half of the Apple builder shim's `fssync` context transfer.
//!
//! `pkg/fssync/walk.go` in `container-builder-shim` asks the client to send the
//! build context as a tar archive: one packet carrying a content checksum,
//! followed by the archive bytes. The checksum names the directory the shim
//! unpacks into and reuses across builds, so it must change whenever the
//! context changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::AppleContainerError;

/// Maximum tar payload carried by a single `BuildTransfer` packet.
///
/// gRPC's default message limit is 4 MiB, and shim 0.8.0 buffers only 32
/// undelivered packets before dropping the stream (`pkg/stream/processor.go`),
/// so chunks are kept large to keep the packet count low.
pub const CONTEXT_CHUNK_SIZE: usize = 512 * 1024;

/// Reject any walk mode the builder shim cannot actually receive.
///
/// `tar` is the only mode implemented by a released shim: `pkg/fssync/walk.go`
/// has a single `ModeTAR` arm and treats an empty mode as tar. Replying in the
/// `json` mode its stale doc comment advertises leaves the shim blocked in
/// `readTarHash` forever, so an unrecognised mode is an error rather than a
/// best-effort reply.
pub fn require_tar_walk_mode(
    metadata: &HashMap<String, String>,
) -> Result<(), AppleContainerError> {
    match metadata.get("mode").map(String::as_str).unwrap_or("") {
        "" | "tar" => Ok(()),
        other => Err(AppleContainerError::XpcError(format!(
            "builder requested unsupported fssync walk mode {other:?}; only \"tar\" is implemented"
        ))),
    }
}

/// The subset of the build context a `Walk` request asks for.
///
/// BuildKit narrows each transfer to the paths it needs: `followpaths` lists
/// exact files (`.dockerignore`, `COPY` sources) and `include-patterns` lists
/// globs. Both arrive comma-separated under the shim's hyphenated metadata
/// keys. An empty filter means the whole context.
///
/// Matching deliberately errs towards including a path: BuildKit filters again
/// on its side, so an extra file only costs transfer time, whereas a missing
/// one breaks the build.
#[derive(Debug, Default)]
pub struct ContextFilter {
    patterns: Vec<String>,
}

impl ContextFilter {
    /// Read the filter out of a `Walk` request's metadata.
    pub fn from_metadata(metadata: &HashMap<String, String>) -> Self {
        let patterns = ["followpaths", "include-patterns"]
            .iter()
            .filter_map(|key| metadata.get(*key))
            .flat_map(|raw| raw.split(','))
            .filter_map(normalize_pattern)
            .collect();
        Self { patterns }
    }

    /// Whether the request asked for the entire context.
    pub fn is_unfiltered(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether a context-relative file path was asked for.
    pub fn matches_file(&self, rel: &str) -> bool {
        self.is_unfiltered() || self.patterns.iter().any(|p| matches_path(p, rel))
    }

    /// Whether a context-relative directory could contain a requested path,
    /// and therefore has to be descended into.
    pub fn matches_dir(&self, rel: &str) -> bool {
        self.is_unfiltered() || self.patterns.iter().any(|p| could_match_within(p, rel))
    }
}

/// Canonicalise one pattern, dropping the ones that select nothing.
fn normalize_pattern(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Match a slash-separated path against a slash-separated glob pattern.
///
/// A pattern that runs out while path segments remain matches, so naming a
/// directory selects everything beneath it.
fn matches_path(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    let Some(head) = pattern.first() else {
        // The pattern named this path or one of its parents.
        return true;
    };
    if *head == "**" {
        return (0..=path.len()).any(|i| match_segments(&pattern[1..], &path[i..]));
    }
    match path.split_first() {
        Some((segment, rest)) if glob_match(head, segment) => match_segments(&pattern[1..], rest),
        _ => false,
    }
}

/// Whether `pattern` could still match something inside directory `dir`.
fn could_match_within(pattern: &str, dir: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    for (index, segment) in dir.split('/').enumerate() {
        match pattern.get(index) {
            // The pattern matched a parent of `dir`, so `dir` is included.
            None => return true,
            // `**` matches any remaining depth.
            Some(&"**") => return true,
            Some(head) if glob_match(head, segment) => {}
            Some(_) => return false,
        }
    }
    true
}

/// Match a single path segment against a glob supporting `*` and `?`.
fn glob_match(pattern: &str, segment: &str) -> bool {
    fn walk(pattern: &[u8], segment: &[u8]) -> bool {
        let Some((head, rest)) = pattern.split_first() else {
            return segment.is_empty();
        };
        match head {
            b'*' => (0..=segment.len()).any(|i| walk(rest, &segment[i..])),
            b'?' => !segment.is_empty() && walk(rest, &segment[1..]),
            literal => segment.first() == Some(literal) && walk(rest, &segment[1..]),
        }
    }
    walk(pattern.as_bytes(), segment.as_bytes())
}

/// One context path selected for transfer.
#[derive(Debug)]
pub struct ContextEntry {
    /// Context-relative, slash-separated, and never prefixed with `./`: the
    /// shim rejects any tar name that does not resolve strictly under its
    /// unpack directory (`pkg/fileutils/tarxfer.go`).
    pub name: String,
    pub path: PathBuf,
    pub metadata: std::fs::Metadata,
}

/// Collect every context path the request selected, in a stable order.
///
/// Symlinks are recorded without being followed, so a link cycle inside the
/// context cannot make this recurse forever.
pub fn collect_context(
    root: &Path,
    filter: &ContextFilter,
) -> Result<Vec<ContextEntry>, AppleContainerError> {
    let mut entries = Vec::new();
    collect_dir(root, root, filter, &mut entries)?;
    Ok(entries)
}

fn collect_dir(
    root: &Path,
    dir: &Path,
    filter: &ContextFilter,
    entries: &mut Vec<ContextEntry>,
) -> Result<(), AppleContainerError> {
    let mut children: Vec<_> = std::fs::read_dir(dir)
        .map_err(AppleContainerError::Io)?
        .filter_map(Result::ok)
        .collect();
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let path = child.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        // `symlink_metadata` describes the link itself, so a symlink is sent
        // as a link rather than being followed out of the context.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        if metadata.is_dir() {
            if !filter.matches_dir(&name) {
                continue;
            }
            entries.push(ContextEntry {
                name,
                path: path.clone(),
                metadata,
            });
            collect_dir(root, &path, filter, entries)?;
        } else {
            if !filter.matches_file(&name) {
                continue;
            }
            entries.push(ContextEntry {
                name,
                path,
                metadata,
            });
        }
    }

    Ok(())
}

/// Build the context tar and return it with the checksum that names it.
///
/// The checksum is a digest of the archive itself, so identical contexts hit
/// the shim's unpack cache and any change misses it.
pub fn build_context_tar(
    root: &Path,
    filter: &ContextFilter,
) -> Result<(String, Vec<u8>), AppleContainerError> {
    let entries = collect_context(root, filter)?;
    let archive = write_tar(&entries)?;
    let checksum = sha256_hex(&archive);
    Ok((checksum, archive))
}

fn write_tar(entries: &[ContextEntry]) -> Result<Vec<u8>, AppleContainerError> {
    use std::os::unix::fs::MetadataExt;

    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(entry.metadata.mode() & 0o7777);
        header.set_uid(entry.metadata.uid() as u64);
        header.set_gid(entry.metadata.gid() as u64);
        header.set_mtime(mtime_secs(&entry.metadata));

        if entry.metadata.is_symlink() {
            let target = std::fs::read_link(&entry.path).map_err(AppleContainerError::Io)?;
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder
                .append_link(&mut header, &entry.name, &target)
                .map_err(AppleContainerError::Io)?;
        } else if entry.metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            builder
                .append_data(&mut header, &entry.name, std::io::empty())
                .map_err(AppleContainerError::Io)?;
        } else {
            let file = std::fs::File::open(&entry.path).map_err(AppleContainerError::Io)?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.metadata.len());
            builder
                .append_data(&mut header, &entry.name, file)
                .map_err(AppleContainerError::Io)?;
        }
    }

    builder.into_inner().map_err(AppleContainerError::Io)
}

/// Seconds since the Unix epoch, or zero for timestamps we cannot read.
pub fn mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a Unix timestamp as RFC 3339, which is what the shim's
/// `time.Parse(time.RFC3339, ...)` transformers require.
pub fn rfc3339_utc(secs: u64) -> String {
    // Days since 1970-01-01 converted with the civil-from-days algorithm.
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Translate a Unix `st_mode` into Go's `fs.FileMode` encoding.
///
/// Go keeps the permission bits in the low nine bits but flags directories and
/// symlinks with its own high bits rather than the Unix file-type nibble, so
/// handing over a raw `st_mode` would tell the shim every path is a regular
/// file.
pub fn go_file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    const GO_MODE_DIR: u32 = 1 << 31;
    const GO_MODE_SYMLINK: u32 = 1 << 27;

    let mut mode = metadata.mode() & 0o777;
    if metadata.is_dir() {
        mode |= GO_MODE_DIR;
    }
    if metadata.is_symlink() {
        mode |= GO_MODE_SYMLINK;
    }
    mode
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The regression behind issue #4's hang: `handle_walk` answered every
    /// request in a `json` mode that no released shim implements, and the
    /// shim's unbuffered tar receiver then blocked forever.
    #[test]
    fn only_the_tar_walk_mode_is_accepted() {
        assert!(require_tar_walk_mode(&metadata(&[("mode", "tar")])).is_ok());
        // An absent or empty mode means tar (`unmarshalWalkMetadata`).
        assert!(require_tar_walk_mode(&metadata(&[])).is_ok());
        assert!(require_tar_walk_mode(&metadata(&[("mode", "")])).is_ok());

        let rejected = require_tar_walk_mode(&metadata(&[("mode", "json")]));
        let message = rejected.expect_err("json mode must be refused").to_string();
        assert!(
            message.contains("json") && message.contains("tar"),
            "the error must name the mode we refused and the one we support; got {message}"
        );
    }

    #[test]
    fn filter_reads_the_shims_hyphenated_metadata_keys() {
        let filter = ContextFilter::from_metadata(&metadata(&[
            ("followpaths", ".dockerignore,app.txt"),
            ("include-patterns", "src"),
        ]));
        assert!(!filter.is_unfiltered());
        assert!(filter.matches_file(".dockerignore"));
        assert!(filter.matches_file("app.txt"));
        assert!(filter.matches_file("src/main.rs"));
        assert!(!filter.matches_file("other.txt"));
    }

    /// The old code read `includePatterns`; the shim sends `include-patterns`.
    #[test]
    fn camel_case_pattern_keys_are_not_mistaken_for_a_filter() {
        let filter = ContextFilter::from_metadata(&metadata(&[("includePatterns", "src")]));
        assert!(
            filter.is_unfiltered(),
            "an unrecognised key must fall back to sending the whole context"
        );
    }

    #[test]
    fn an_absent_or_empty_filter_selects_everything() {
        assert!(ContextFilter::from_metadata(&metadata(&[])).is_unfiltered());
        assert!(ContextFilter::from_metadata(&metadata(&[
            ("followpaths", ""),
            ("include-patterns", "")
        ]))
        .is_unfiltered());
        assert!(ContextFilter::default().matches_file("anything/at/all"));
    }

    #[test]
    fn directories_are_descended_when_they_could_hold_a_match() {
        let filter =
            ContextFilter::from_metadata(&metadata(&[("followpaths", "src/deep/app.txt")]));
        assert!(filter.matches_dir("src"));
        assert!(filter.matches_dir("src/deep"));
        assert!(!filter.matches_dir("vendor"));
        assert!(filter.matches_file("src/deep/app.txt"));
    }

    #[test]
    fn glob_patterns_match_within_and_across_segments() {
        let stars = ContextFilter::from_metadata(&metadata(&[("include-patterns", "*.txt")]));
        assert!(stars.matches_file("app.txt"));
        assert!(!stars.matches_file("app.log"));

        let recursive =
            ContextFilter::from_metadata(&metadata(&[("include-patterns", "**/*.txt")]));
        assert!(recursive.matches_file("app.txt"));
        assert!(recursive.matches_file("src/deep/app.txt"));
        assert!(
            recursive.matches_dir("src"),
            "a `**` pattern must not stop the walk from descending"
        );

        let single = ContextFilter::from_metadata(&metadata(&[("include-patterns", "a?c.txt")]));
        assert!(single.matches_file("abc.txt"));
        assert!(!single.matches_file("ac.txt"));
    }

    #[test]
    fn naming_a_directory_selects_everything_beneath_it() {
        let filter = ContextFilter::from_metadata(&metadata(&[("followpaths", "src")]));
        assert!(filter.matches_file("src/deep/app.txt"));
        assert!(filter.matches_dir("src/deep"));
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("context subdirectory");
        }
        std::fs::write(path, contents).expect("context file");
    }

    fn tar_names(archive: &[u8]) -> Vec<String> {
        tar::Archive::new(archive)
            .entries()
            .expect("archive entries")
            .map(|entry| {
                entry
                    .expect("entry")
                    .path()
                    .expect("entry path")
                    .to_string_lossy()
                    .to_string()
            })
            .collect()
    }

    /// Dotfiles were skipped outright, which dropped exactly the paths
    /// BuildKit asks for first (`.dockerignore`) and the devcontainer config.
    #[test]
    fn the_context_tar_includes_dotfiles() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), ".dockerignore", "*.log\n");
        write(dir.path(), ".devcontainer/devcontainer.json", "{}");
        write(dir.path(), "app.txt", "payload");

        let (_, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        let names = tar_names(&archive);

        assert!(names.contains(&".dockerignore".to_string()), "{names:?}");
        assert!(
            names.contains(&".devcontainer/devcontainer.json".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"app.txt".to_string()), "{names:?}");
    }

    /// `pkg/fileutils/tarxfer.go` rejects any entry that does not resolve
    /// strictly under the unpack directory, and `./` resolves to the directory
    /// itself — the observed `invalid tar path: ./` failure.
    #[test]
    fn tar_entry_names_are_relative_with_no_dot_slash_prefix() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "nested/app.txt", "payload");

        let (_, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");

        for name in tar_names(&archive) {
            assert!(!name.starts_with("./"), "entry {name:?} has a ./ prefix");
            assert!(!name.starts_with('/'), "entry {name:?} is absolute");
            assert_ne!(name, ".", "a `.` root entry is rejected by the shim");
        }
    }

    #[test]
    fn the_tar_carries_only_the_requested_paths() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), ".dockerignore", "*.log\n");
        write(dir.path(), "app.txt", "payload");
        write(dir.path(), "vendor/huge.bin", "lots");

        let filter = ContextFilter::from_metadata(&metadata(&[("followpaths", ".dockerignore")]));
        let (_, archive) = build_context_tar(dir.path(), &filter).expect("context tar");

        assert_eq!(tar_names(&archive), vec![".dockerignore".to_string()]);
    }

    /// The shim caches its unpacked context under the checksum we send, so a
    /// stale checksum would silently rebuild against the previous context.
    #[test]
    fn the_checksum_tracks_the_context_contents() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "before");
        let (first, _) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");

        let (repeat, _) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        assert_eq!(first, repeat, "an unchanged context must hit the cache");

        write(dir.path(), "app.txt", "after!");
        let (changed, _) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        assert_ne!(first, changed, "a changed context must miss the cache");

        assert_eq!(first.len(), 64, "checksum must be a hex sha256: {first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn an_empty_context_still_produces_a_readable_archive() {
        let dir = tempfile::tempdir().expect("temp context");
        let (checksum, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");

        assert!(!checksum.is_empty());
        assert!(
            !archive.is_empty(),
            "the shim blocks until at least one data packet arrives"
        );
        assert!(tar_names(&archive).is_empty());
    }

    #[test]
    fn symlinks_are_recorded_without_being_followed() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        std::os::unix::fs::symlink("app.txt", dir.path().join("link.txt")).expect("symlink");

        let (_, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        let mut archive = tar::Archive::new(archive.as_slice());
        let link = archive
            .entries()
            .expect("entries")
            .map(|e| e.expect("entry"))
            .find(|e| e.path().expect("path").to_string_lossy() == "link.txt")
            .expect("the symlink must be present");

        assert_eq!(link.header().entry_type(), tar::EntryType::Symlink);
        assert_eq!(
            link.link_name().expect("link name").expect("target"),
            Path::new("app.txt")
        );
    }

    /// A symlink loop inside the context must not make the walk recurse away.
    #[test]
    fn a_symlink_cycle_terminates() {
        let dir = tempfile::tempdir().expect("temp context");
        std::fs::create_dir(dir.path().join("sub")).expect("subdirectory");
        std::os::unix::fs::symlink(dir.path(), dir.path().join("sub/loop")).expect("symlink");

        let (_, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        assert!(tar_names(&archive).contains(&"sub/loop".to_string()));
    }

    #[test]
    fn go_file_mode_flags_directories_and_links_the_way_go_encodes_them() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        std::os::unix::fs::symlink("app.txt", dir.path().join("link.txt")).expect("symlink");

        let file = std::fs::symlink_metadata(dir.path().join("app.txt")).expect("file metadata");
        let directory = std::fs::symlink_metadata(dir.path()).expect("dir metadata");
        let link = std::fs::symlink_metadata(dir.path().join("link.txt")).expect("link metadata");

        assert_eq!(
            go_file_mode(&file) & !0o777,
            0,
            "a file carries no type bits"
        );
        assert_eq!(go_file_mode(&directory) & (1 << 31), 1 << 31);
        assert_eq!(go_file_mode(&link) & (1 << 27), 1 << 27);
    }

    /// The shim parses these with `time.Parse(time.RFC3339, ...)`, which
    /// rejects a bare Unix timestamp.
    #[test]
    fn timestamps_are_formatted_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_utc(1_784_611_326), "2026-07-21T05:22:06Z");
    }
}
