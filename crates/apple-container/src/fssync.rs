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
/// exact files (`.dockerignore`, `COPY` sources), `include-patterns` lists
/// globs, and `exclude-patterns` carries the `.dockerignore` rules BuildKit
/// already parsed. All arrive comma-separated under the shim's hyphenated
/// metadata keys. An empty filter means the whole context.
///
/// Inclusion deliberately errs towards sending a path: BuildKit filters again
/// on its side, so an extra file only costs transfer time, whereas a missing
/// one breaks the build. Exclusion errs the same way — a path is only dropped
/// when a rule clearly names it — but honouring the rules at all is what keeps
/// `.git`, `target` and `node_modules` out of every transfer.
#[derive(Debug, Default, Clone)]
pub struct ContextFilter {
    patterns: Vec<String>,
    excludes: Vec<ExcludeRule>,
}

/// One `.dockerignore` rule, with the `!` re-include form kept apart.
#[derive(Debug, Clone)]
struct ExcludeRule {
    pattern: String,
    negated: bool,
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
        let excludes = metadata
            .get("exclude-patterns")
            .into_iter()
            .flat_map(|raw| raw.split(','))
            .filter_map(normalize_exclude)
            .collect();
        Self { patterns, excludes }
    }

    /// Whether the request asked for the entire context.
    ///
    /// Only the tests distinguish the two: the walk itself asks about one path
    /// at a time, so nothing in the transfer path needs to know.
    #[cfg(test)]
    pub fn is_unfiltered(&self) -> bool {
        self.patterns.is_empty() && self.excludes.is_empty()
    }

    /// Whether a context-relative file path was asked for.
    pub fn matches_file(&self, rel: &str) -> bool {
        self.is_included(rel) && !self.is_excluded(rel)
    }

    /// Whether a context-relative directory could contain a requested path,
    /// and therefore has to be descended into.
    pub fn matches_dir(&self, rel: &str) -> bool {
        let reachable =
            self.patterns.is_empty() || self.patterns.iter().any(|p| could_match_within(p, rel));
        reachable && !self.is_pruned(rel)
    }

    fn is_included(&self, rel: &str) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|p| matches_path(p, rel))
    }

    /// Whether the `.dockerignore` rules drop this path.
    ///
    /// Docker resolves conflicting rules by last match, so a trailing
    /// `!keep/this` re-includes what an earlier rule excluded.
    fn is_excluded(&self, rel: &str) -> bool {
        let mut excluded = false;
        for rule in &self.excludes {
            if matches_path(&rule.pattern, rel) {
                excluded = !rule.negated;
            }
        }
        excluded
    }

    /// Whether an excluded directory can be skipped outright.
    ///
    /// Only when no re-include rule could still match something inside it —
    /// `target` with a trailing `!target/keep.txt` still has to be descended.
    fn is_pruned(&self, rel: &str) -> bool {
        self.is_excluded(rel)
            && !self
                .excludes
                .iter()
                .any(|rule| rule.negated && could_match_within(&rule.pattern, rel))
    }
}

/// Canonicalise one `.dockerignore` rule, keeping its `!` re-include sense.
fn normalize_exclude(raw: &str) -> Option<ExcludeRule> {
    let trimmed = raw.trim();
    let (negated, pattern) = match trimmed.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    Some(ExcludeRule {
        pattern: normalize_pattern(pattern)?,
        negated,
    })
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

/// Match path segments against pattern segments, `**` matching any depth.
///
/// Walked with a single backtrack point rather than by recursing at every `**`:
/// a rule such as `**/a/**/a/**/b` from a `.dockerignore` would otherwise cost
/// exponential time against a deep path, and the walk runs on a blocking-pool
/// thread with no deadline above it.
fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    let (mut p, mut s) = (0, 0);
    // Where to resume from, and how much of the path the last `**` has eaten.
    let mut wildcard: Option<usize> = None;
    let mut consumed = 0;

    loop {
        // The pattern named this path or one of its parents.
        if p == pattern.len() {
            return true;
        }
        if pattern[p] == "**" {
            wildcard = Some(p);
            consumed = s;
            p += 1;
            continue;
        }
        if s < path.len() && glob_match(pattern[p], path[s]) {
            p += 1;
            s += 1;
            continue;
        }
        match wildcard {
            // Let the `**` swallow one more segment and try the rest again.
            Some(star) if consumed < path.len() => {
                p = star + 1;
                consumed += 1;
                s = consumed;
            }
            _ => return false,
        }
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
///
/// Backtracks to the most recent `*` rather than branching at each one: a
/// `.dockerignore` rule such as `*a*a*a*a*a*a*a*a*b` would otherwise take
/// exponential time against a long filename and pin a blocking-pool thread for
/// the rest of the build.
fn glob_match(pattern: &str, segment: &str) -> bool {
    let pattern = pattern.as_bytes();
    let segment = segment.as_bytes();
    let (mut p, mut s) = (0, 0);
    // Where to resume from, and how much of the segment the last `*` has eaten.
    let mut wildcard: Option<usize> = None;
    let mut consumed = 0;

    while s < segment.len() {
        match pattern.get(p) {
            Some(b'*') => {
                wildcard = Some(p);
                consumed = s;
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                s += 1;
            }
            Some(literal) if *literal == segment[s] => {
                p += 1;
                s += 1;
            }
            // Let the `*` swallow one more byte and try the rest again.
            _ => match wildcard {
                Some(star) => {
                    p = star + 1;
                    consumed += 1;
                    s = consumed;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}

/// One context path selected for transfer.
#[derive(Debug)]
pub struct ContextEntry {
    /// Context-relative, slash-separated, and never prefixed with `./`: the
    /// shim rejects any tar name that does not resolve strictly under its
    /// unpack directory (`pkg/fileutils/tarxfer.go`).
    ///
    /// The separators are the ones the filesystem gave: a backslash is a legal
    /// byte in a macOS filename, so rewriting it would turn a file honestly
    /// named `..\..\etc\passwd` into the traversal `../../etc/passwd` and leave
    /// only the shim's own validation between a crafted name and a write
    /// outside its unpack directory. It would relocate innocent files too —
    /// `a\b.txt` would be stored as `a/b.txt`, under a directory the context
    /// never had.
    pub name: String,
    pub path: PathBuf,
    pub metadata: std::fs::Metadata,
}

/// How deep the walk will descend into a build context.
///
/// Far past any real project layout, and shallow enough that a pathological
/// tree fails with a message instead of overflowing the stack and aborting the
/// whole process.
const MAX_CONTEXT_DEPTH: usize = 128;

/// How many paths the walk will select out of a build context.
///
/// The walk holds one entry per selected path — a name, a path and a `stat`
/// struct — for the whole transfer, so breadth costs resident memory the way
/// depth costs stack. An unfiltered monorepo checkout with a vendored tree and
/// no `.dockerignore` is the realistic way to reach this, and a build that
/// stops with the count and the advice to write a `.dockerignore` is a far
/// better outcome than one that is killed for its resident size.
const MAX_CONTEXT_ENTRIES: usize = 500_000;

/// Collect every context path the request selected, in a stable order.
///
/// Symlinks are recorded without being followed, so a link cycle inside the
/// context cannot make this recurse forever.
pub fn collect_context(
    root: &Path,
    filter: &ContextFilter,
) -> Result<Vec<ContextEntry>, AppleContainerError> {
    let mut entries = Vec::new();
    collect_dir(root, root, filter, 0, &mut entries)?;
    Ok(entries)
}

/// Collect a directory listing, keeping any failure part-way through it.
///
/// `read_dir` can succeed and then fail while it is being walked, and dropping
/// those entries would leave the image missing files with nothing said about
/// them. Taken as an iterator so the failure can be exercised: the filesystem
/// cannot be made to fail mid-listing on demand.
fn listed_children<I>(listing: I) -> Result<Vec<std::fs::DirEntry>, AppleContainerError>
where
    I: IntoIterator<Item = std::io::Result<std::fs::DirEntry>>,
{
    listing
        .into_iter()
        .map(|child| child.map_err(AppleContainerError::Io))
        .collect()
}

fn collect_dir(
    root: &Path,
    dir: &Path,
    filter: &ContextFilter,
    depth: usize,
    entries: &mut Vec<ContextEntry>,
) -> Result<(), AppleContainerError> {
    if depth > MAX_CONTEXT_DEPTH {
        return Err(AppleContainerError::XpcError(format!(
            "build context is nested more than {MAX_CONTEXT_DEPTH} directories deep at {}",
            dir.display()
        )));
    }

    let mut children = listed_children(std::fs::read_dir(dir).map_err(AppleContainerError::Io)?)?;
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let path = child.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        // Only ever used to decide against a path, so a lossy reading of it is
        // safe: nothing is transferred under this name. The name a selected
        // entry is stored under comes from `selected_name`.
        let candidate = relative.to_string_lossy();
        // `symlink_metadata` describes the link itself, so a symlink is sent
        // as a link rather than being followed out of the context.
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // A path that vanished between the listing and the stat cannot be
            // sent and is not a failure. Anything else — EACCES on an
            // untraversable directory, EIO — would silently drop a file the
            // image needs, so it stops the walk instead.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(AppleContainerError::Io(e)),
        };

        if metadata.is_dir() {
            if !filter.matches_dir(&candidate) {
                continue;
            }
            let name = selected_name(relative, &path)?;
            push_entry(
                entries,
                ContextEntry {
                    name,
                    path: path.clone(),
                    metadata,
                },
            )?;
            collect_dir(root, &path, filter, depth + 1, entries)?;
        } else {
            // Only regular files and symlinks have contents a tar can carry.
            // Opening a FIFO with no writer blocks forever and a socket fails
            // outright, so a stray one in the context would hang or break the
            // build rather than being skipped the way BuildKit's own fsutil
            // skips it.
            if !metadata.is_file() && !metadata.is_symlink() {
                continue;
            }
            if !filter.matches_file(&candidate) {
                continue;
            }
            let name = selected_name(relative, &path)?;
            push_entry(
                entries,
                ContextEntry {
                    name,
                    path,
                    metadata,
                },
            )?;
        }
    }

    Ok(())
}

/// The archive name for a path the walk has decided to send.
///
/// `to_string_lossy` would mint a `U+FFFD` name whose bytes name a different
/// path than the one being read, so a name this crate cannot carry stops the
/// walk instead of being sent under a wrong one. Only for a path that is
/// actually selected, though: raising it before the type check and the
/// `.dockerignore` filter would fail the whole build over a stray socket or an
/// excluded artifact, and leave the user with advice that cannot work.
fn selected_name(relative: &Path, path: &Path) -> Result<String, AppleContainerError> {
    relative.to_str().map(str::to_string).ok_or_else(|| {
        AppleContainerError::XpcError(format!(
            "build context path {} is not valid UTF-8 and cannot be named in the archive; \
             exclude it with a .dockerignore",
            path.display()
        ))
    })
}

/// Record one selected path, refusing a context wider than the walk will hold.
fn push_entry(
    entries: &mut Vec<ContextEntry>,
    entry: ContextEntry,
) -> Result<(), AppleContainerError> {
    if entries.len() >= MAX_CONTEXT_ENTRIES {
        return Err(AppleContainerError::XpcError(format!(
            "build context selects more than {MAX_CONTEXT_ENTRIES} paths; \
             exclude what the build does not need with a .dockerignore"
        )));
    }
    entries.push(entry);
    Ok(())
}

/// Build the context tar and return it with the checksum that names it.
///
/// Holds the whole archive in memory, so a real transfer must never use it —
/// a repository-sized context would cost a multi-gigabyte allocation. It exists
/// so the tests can compare what [`stream_context_tar`] handed over against a
/// single reference archive, and is confined to them so it cannot be picked up
/// by the transfer path again.
#[cfg(test)]
pub fn build_context_tar(
    root: &Path,
    filter: &ContextFilter,
) -> Result<(String, Vec<u8>), AppleContainerError> {
    let entries = collect_context(root, filter)?;
    let mut archive = Vec::new();
    write_tar(&entries, &mut archive)?;
    let checksum = sha256_hex(&archive);
    Ok((checksum, archive))
}

/// The checksum that names this context's unpack directory in the shim.
///
/// A digest of the archive itself, so identical contexts hit the shim's unpack
/// cache and any change misses it. The archive is hashed as it is produced
/// rather than assembled first, so a multi-gigabyte context costs a digest
/// rather than a multi-gigabyte allocation.
pub fn context_tar_checksum(entries: &[ContextEntry]) -> Result<String, AppleContainerError> {
    let mut digest = Sha256Sink::default();
    write_tar(entries, &mut digest)?;
    Ok(digest.digest.finish())
}

/// Running digest of an archive as it is handed over.
///
/// The same definition the announced checksum is computed with, so a caller can
/// check the bytes it actually transferred against the hash the shim was
/// promised without the two drifting apart.
#[derive(Default)]
pub struct ArchiveDigest {
    hasher: sha2::Sha256,
}

impl ArchiveDigest {
    pub fn update(&mut self, chunk: &[u8]) {
        use sha2::Digest;
        self.hasher.update(chunk);
    }

    pub fn finish(self) -> String {
        use sha2::Digest;
        hex::encode(self.hasher.finalize())
    }
}

/// Write the context tar out in [`CONTEXT_CHUNK_SIZE`] pieces.
///
/// `emit` is handed one chunk at a time and is expected to hand it straight to
/// the transfer, so peak memory is one chunk plus whatever the caller queues —
/// not the whole context. Blocking file reads happen on the calling thread, so
/// callers on an async runtime belong on the blocking pool.
pub fn stream_context_tar(
    entries: &[ContextEntry],
    emit: &mut dyn FnMut(Vec<u8>) -> Result<(), AppleContainerError>,
) -> Result<(), AppleContainerError> {
    let mut chunks = ChunkWriter::new(emit);
    match write_tar(entries, &mut chunks) {
        Ok(()) => chunks.flush_remainder(),
        // `tar::Builder` only surfaces an `io::Error`, so a sink failure comes
        // back re-wrapped; the sink kept the one it actually raised.
        Err(wrapped) => Err(chunks.failure.take().unwrap_or(wrapped)),
    }
}

/// A `Write` sink that hashes without keeping anything.
#[derive(Default)]
struct Sha256Sink {
    digest: ArchiveDigest,
}

impl std::io::Write for Sha256Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.digest.update(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `Write` sink that hands off fixed-size chunks as they fill.
struct ChunkWriter<'a> {
    emit: &'a mut dyn FnMut(Vec<u8>) -> Result<(), AppleContainerError>,
    buffer: Vec<u8>,
    failure: Option<AppleContainerError>,
}

impl<'a> ChunkWriter<'a> {
    fn new(emit: &'a mut dyn FnMut(Vec<u8>) -> Result<(), AppleContainerError>) -> Self {
        Self {
            emit,
            buffer: Vec::with_capacity(CONTEXT_CHUNK_SIZE),
            failure: None,
        }
    }

    /// Hand off whatever the last chunk boundary left behind.
    ///
    /// Only reached once the archive was written in full, so any `failure` the
    /// sink recorded has already been returned by [`stream_context_tar`].
    fn flush_remainder(mut self) -> Result<(), AppleContainerError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let last = std::mem::take(&mut self.buffer);
        (self.emit)(last)
    }
}

impl std::io::Write for ChunkWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(data);
        while self.buffer.len() >= CONTEXT_CHUNK_SIZE {
            let remainder = self.buffer.split_off(CONTEXT_CHUNK_SIZE);
            let chunk = std::mem::replace(&mut self.buffer, remainder);
            if let Err(e) = (self.emit)(chunk) {
                // `tar::Builder` only surfaces an `io::Error`, so the real
                // cause is kept for `flush_remainder` to return.
                let reported = std::io::Error::other(e.to_string());
                self.failure = Some(e);
                return Err(reported);
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_tar<W: std::io::Write>(
    entries: &[ContextEntry],
    out: W,
) -> Result<(), AppleContainerError> {
    use std::os::unix::fs::MetadataExt;

    let mut builder = tar::Builder::new(out);
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
            // Both the size and the type come from the open file rather than
            // from the metadata the walk captured: an editor or `cargo`
            // rewriting a context file between the two would otherwise put a
            // length in the header that does not describe what follows it, and
            // a path that stopped being a regular file has to be reported
            // rather than dropped from the archive with nothing said.
            let (file, opened) = open_regular(&entry.path, &entry.name)?;
            let size = opened.len();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(size);
            builder
                .append_data(&mut header, &entry.name, ExactLength::new(file, size))
                .map_err(AppleContainerError::Io)?;
        }
    }

    builder.into_inner().map_err(AppleContainerError::Io)?;
    Ok(())
}

/// Open a context file, refusing anything that is no longer a regular file.
///
/// The walk selects only regular files, but the transfer opens them later, and
/// a path replaced by a FIFO in between would block this thread forever with no
/// deadline above it. `O_NONBLOCK` makes the open return whatever the path has
/// become so it can be reported, and is a no-op for the regular file this
/// almost always is.
pub fn open_regular(
    path: &Path,
    name: &str,
) -> Result<(std::fs::File, std::fs::Metadata), AppleContainerError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(AppleContainerError::Io)?;
    let opened = file.metadata().map_err(AppleContainerError::Io)?;
    if !opened.is_file() {
        return Err(AppleContainerError::XpcError(format!(
            "{name} stopped being a regular file while the build context was being sent"
        )));
    }
    Ok((file, opened))
}

/// A reader that yields exactly the number of bytes its entry's header declares.
///
/// `tar::Builder` pads each entry from the bytes it actually copied rather than
/// from the header it just wrote, so a file whose length changes between the
/// two shifts every following entry: the shim's extractor then reads part of
/// the file's contents as the next 512-byte header and fails with garbage
/// entries or `invalid tar path`. Truncating a file that grew and zero-filling
/// one that shrank keeps the header and the stream describing the same thing
/// whatever the filesystem does underneath.
struct ExactLength<R> {
    inner: R,
    remaining: u64,
}

impl<R> ExactLength<R> {
    fn new(inner: R, length: u64) -> Self {
        Self {
            inner,
            remaining: length,
        }
    }
}

impl<R: std::io::Read> std::io::Read for ExactLength<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let wanted = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        if wanted == 0 {
            return Ok(0);
        }
        let buf = &mut buf[..wanted];
        let read = self.inner.read(buf)?;
        if read == 0 {
            // The file ended early; the header already promised these bytes.
            buf.fill(0);
            self.remaining -= wanted as u64;
            return Ok(wanted);
        }
        self.remaining -= read as u64;
        Ok(read)
    }
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

#[cfg(test)]
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
        assert!(
            ContextFilter::from_metadata(&metadata(&[
                ("followpaths", ""),
                ("include-patterns", "")
            ]))
            .is_unfiltered()
        );
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

    /// `.dockerignore` reaches the client as `exclude-patterns`. Ignoring the
    /// key tarred `.git`, `target` and `node_modules` into every build.
    #[test]
    fn exclude_patterns_drop_the_paths_dockerignore_names() {
        let filter =
            ContextFilter::from_metadata(&metadata(&[("exclude-patterns", ".git,target,*.log")]));

        assert!(!filter.is_unfiltered(), "excludes alone are still a filter");
        assert!(filter.matches_file("src/main.rs"));
        assert!(!filter.matches_file("target/debug/dev"));
        assert!(!filter.matches_file("app.log"));
        assert!(
            !filter.matches_dir("target"),
            "an excluded directory must not be descended into"
        );
        assert!(!filter.matches_dir(".git"));
        assert!(filter.matches_dir("src"));
    }

    /// Docker resolves conflicting rules by last match, so a trailing `!` rule
    /// re-includes — and the directory holding it still has to be walked.
    #[test]
    fn a_negated_exclude_re_includes_what_an_earlier_rule_dropped() {
        let filter = ContextFilter::from_metadata(&metadata(&[(
            "exclude-patterns",
            "target,!target/keep.txt",
        )]));

        assert!(!filter.matches_file("target/debug/dev"));
        assert!(filter.matches_file("target/keep.txt"));
        assert!(
            filter.matches_dir("target"),
            "a directory holding a re-included path must still be descended"
        );
    }

    /// Includes and excludes compose: an excluded path stays out even when an
    /// include pattern names it.
    #[test]
    fn an_exclude_overrides_an_include_pattern() {
        let filter = ContextFilter::from_metadata(&metadata(&[
            ("include-patterns", "src"),
            ("exclude-patterns", "src/generated"),
        ]));

        assert!(filter.matches_file("src/main.rs"));
        assert!(!filter.matches_file("src/generated/api.rs"));
        assert!(!filter.matches_file("docs/readme.md"));
    }

    #[test]
    fn excluded_paths_never_reach_the_archive() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        write(dir.path(), "target/debug/huge.bin", "lots");
        write(dir.path(), ".git/config", "[core]");

        let filter =
            ContextFilter::from_metadata(&metadata(&[("exclude-patterns", "target,.git")]));
        let (_, archive) = build_context_tar(dir.path(), &filter).expect("context tar");

        assert_eq!(tar_names(&archive), vec!["app.txt".to_string()]);
    }

    /// The transfer must not materialise the whole context first: the archive
    /// is handed over in bounded chunks, and those chunks must reassemble into
    /// exactly the archive the checksum names.
    #[test]
    fn the_archive_streams_in_bounded_chunks_that_match_the_checksum() {
        let dir = tempfile::tempdir().expect("temp context");
        // Larger than one chunk, so the streaming path is actually exercised.
        write(
            dir.path(),
            "big.bin",
            &"0123456789abcdef".repeat(CONTEXT_CHUNK_SIZE / 8),
        );
        write(dir.path(), "app.txt", "payload");

        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");
        let checksum = context_tar_checksum(&entries).expect("checksum");

        let mut chunks: Vec<Vec<u8>> = Vec::new();
        stream_context_tar(&entries, &mut |chunk| {
            chunks.push(chunk);
            Ok(())
        })
        .expect("stream");

        assert!(
            chunks.len() > 1,
            "a large context must not arrive as one packet"
        );
        assert!(
            chunks.iter().all(|c| c.len() <= CONTEXT_CHUNK_SIZE),
            "no chunk may exceed the transfer limit"
        );

        let streamed: Vec<u8> = chunks.concat();
        let (buffered_checksum, buffered) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");
        assert_eq!(checksum, buffered_checksum);
        assert_eq!(
            streamed, buffered,
            "the stream must be the archive we hashed"
        );
        assert_eq!(sha256_hex(&streamed), checksum);
    }

    /// An empty context produces only the end-of-archive marker, which still
    /// has to be handed over — the shim blocks until a data packet arrives.
    #[test]
    fn streaming_an_empty_context_still_emits_the_archive_marker() {
        let dir = tempfile::tempdir().expect("temp context");
        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");

        let mut chunks: Vec<Vec<u8>> = Vec::new();
        stream_context_tar(&entries, &mut |chunk| {
            chunks.push(chunk);
            Ok(())
        })
        .expect("stream");

        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].is_empty());
    }

    /// A transfer that has gone away must surface as an error rather than as a
    /// silently truncated archive the shim would unpack as the real context.
    #[test]
    fn a_failing_sink_stops_the_stream_with_its_own_error() {
        let dir = tempfile::tempdir().expect("temp context");
        write(
            dir.path(),
            "big.bin",
            &"0123456789abcdef".repeat(CONTEXT_CHUNK_SIZE / 8),
        );
        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");

        let outcome = stream_context_tar(&entries, &mut |_| {
            Err(AppleContainerError::XpcError(
                "receiver is gone".to_string(),
            ))
        });

        let error = outcome.expect_err("a failing sink must fail the walk");
        // `tar::Builder` can only carry an `io::Error`, so the sink's own error
        // has to be recovered rather than reported as a filesystem failure the
        // caller would go looking for on disk.
        assert!(
            matches!(error, AppleContainerError::XpcError(_)),
            "the sink's own error variant must survive, got {error:?}"
        );
        assert!(error.to_string().contains("receiver is gone"), "{error}");
    }

    /// A genuine filesystem failure must still arrive as one: recovering the
    /// sink's error must not swallow the case where the sink was fine and the
    /// context itself could not be read.
    #[test]
    fn a_read_failure_still_reports_as_an_io_error() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");
        std::fs::remove_file(dir.path().join("app.txt")).expect("remove");

        let error = stream_context_tar(&entries, &mut |_| Ok(()))
            .expect_err("a context file that vanished must fail the walk");
        assert!(
            matches!(error, AppleContainerError::Io(_)),
            "a filesystem failure must stay an Io error, got {error:?}"
        );
    }

    /// The header is written from the file's length and the entry's payload is
    /// padded from the bytes actually copied, so the two must describe the same
    /// file. A context file that grew between the walk and the transfer used to
    /// overrun its own entry, leaving the shim's extractor parsing file
    /// contents as the next tar header.
    #[test]
    fn a_file_that_changed_since_the_walk_still_produces_a_readable_archive() {
        for (before, after) in [("small", "much longer than before"), ("longer start", "s")] {
            let dir = tempfile::tempdir().expect("temp context");
            write(dir.path(), "app.txt", before);
            write(dir.path(), "zz-after.txt", "sentinel");
            let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");

            // Rewritten after the walk captured its metadata, exactly as an
            // editor or a build running in the workspace would.
            write(dir.path(), "app.txt", after);

            let mut archive = Vec::new();
            stream_context_tar(&entries, &mut |chunk| {
                archive.extend_from_slice(&chunk);
                Ok(())
            })
            .expect("stream");

            assert_eq!(
                tar_names(&archive),
                vec!["app.txt".to_string(), "zz-after.txt".to_string()],
                "a resized file must not corrupt the entries after it"
            );
            let mut reader = tar::Archive::new(archive.as_slice());
            let sizes: Vec<u64> = reader
                .entries()
                .expect("entries")
                .map(|e| e.expect("entry").header().size().expect("size"))
                .collect();
            assert_eq!(
                sizes[0],
                after.len() as u64,
                "the header must describe the bytes that were actually sent"
            );
        }
    }

    /// Opening a FIFO for reading blocks until something writes to it, and the
    /// walk runs on a blocking-pool thread with no deadline above it — so a
    /// stray pipe or socket in the context would hang `dev up` outright.
    #[test]
    fn non_regular_files_are_left_out_of_the_context() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        let fifo = std::ffi::CString::new(
            dir.path()
                .join("dev-server.fifo")
                .to_string_lossy()
                .as_bytes(),
        )
        .expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0, "mkfifo");

        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["app.txt"],
            "a named pipe must not be opened by the transfer"
        );
    }

    /// A name the archive cannot carry stops the walk rather than being sent
    /// under a lossy one whose bytes name a different path. The advice it gives
    /// is only actionable because the guard runs after `matches_file` and the
    /// type check, so an excluded artifact never reaches it.
    #[test]
    fn a_name_that_is_not_utf8_is_refused_with_advice_that_works() {
        use std::os::unix::ffi::OsStrExt;

        let relative = Path::new(std::ffi::OsStr::from_bytes(b"vendor/bad\xff.iso"));
        let error = selected_name(relative, Path::new("/ctx/vendor/bad.iso"))
            .expect_err("a name the archive cannot carry must stop the walk");
        assert!(error.to_string().contains(".dockerignore"), "{error}");

        assert_eq!(
            selected_name(Path::new("a/b.txt"), Path::new("/ctx/a/b.txt"))
                .expect("a name the archive can carry"),
            "a/b.txt"
        );
    }

    /// A file that cannot be stat'ed is a file that would be missing from the
    /// image with nothing said about it, so the walk has to fail instead.
    /// A path that merely vanished is not that: it cannot be sent either way.
    ///
    /// Mode 0o444 is the case that reaches the stat: the directory can be
    /// listed, so `read_dir` succeeds and every child is named, but without the
    /// execute bit none of them can be stat'ed. A directory that cannot be
    /// listed at all fails one step earlier and proves nothing about this.
    #[test]
    fn a_child_that_cannot_be_stated_fails_the_walk_rather_than_being_dropped() {
        use std::os::unix::fs::PermissionsExt;

        // Permissions do not apply to root, so there is nothing to observe.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "secret/app.txt", "payload");
        let secret = dir.path().join("secret");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o444)).expect("chmod");

        let outcome = collect_context(dir.path(), &ContextFilter::default());

        // Restore before asserting so the temporary directory can be removed.
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = outcome
            .expect_err("a context file that cannot be stat'ed must not be silently omitted");
        assert!(
            matches!(error, AppleContainerError::Io(_)),
            "the filesystem's own refusal must survive, got {error:?}"
        );
    }

    /// A listing that fails part-way through must fail the walk. Dropping the
    /// failed entry would leave the image missing a file with nothing said —
    /// the same silent omission an unreadable stat would cause, one step
    /// earlier. The filesystem cannot be made to fail mid-listing on demand,
    /// so the failure is handed to the collector directly.
    #[test]
    fn a_listing_that_fails_part_way_through_fails_the_walk() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");

        let readable = listed_children(std::fs::read_dir(dir.path()).expect("listing"))
            .expect("a listing that does not fail must be collected");
        assert_eq!(readable.len(), 1);

        let interrupted = std::fs::read_dir(dir.path())
            .expect("listing")
            .chain(std::iter::once(Err(std::io::Error::other("EIO"))));
        let error = listed_children(interrupted)
            .expect_err("an entry that cannot be listed must not be silently dropped");
        assert!(
            matches!(error, AppleContainerError::Io(_)),
            "the filesystem's own failure must survive, got {error:?}"
        );
    }

    /// A path replaced by a named pipe after the walk selected it must be
    /// reported: opening it would otherwise block this thread forever, and
    /// leaving it out would drop a file the image needs with nothing said.
    #[test]
    fn a_file_that_became_a_pipe_after_the_walk_is_reported() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), "app.txt", "payload");
        let entries = collect_context(dir.path(), &ContextFilter::default()).expect("walk");

        std::fs::remove_file(dir.path().join("app.txt")).expect("remove");
        let fifo = std::ffi::CString::new(dir.path().join("app.txt").to_string_lossy().as_bytes())
            .expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0, "mkfifo");

        let error = stream_context_tar(&entries, &mut |_| Ok(()))
            .expect_err("a path that stopped being a regular file must fail the walk");
        assert!(
            error.to_string().contains("stopped being a regular file"),
            "the error must name the type change, got {error}"
        );
    }

    /// A pathological tree must come back as an error rather than overflow the
    /// stack, which would abort the process instead of failing the build.
    #[test]
    fn a_context_nested_past_the_depth_limit_is_reported() {
        let dir = tempfile::tempdir().expect("temp context");
        let mut deep = dir.path().to_path_buf();
        for _ in 0..(MAX_CONTEXT_DEPTH + 2) {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).expect("deep tree");

        let error = collect_context(dir.path(), &ContextFilter::default())
            .expect_err("an unbounded tree must be refused");
        assert!(error.to_string().contains("deep"), "{error}");
    }

    /// Backtracking one `*` at a time keeps a hostile `.dockerignore` rule from
    /// pinning a blocking-pool thread; the recursive form took exponential time
    /// on exactly this shape.
    #[test]
    fn a_pathological_glob_still_matches_promptly() {
        let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let segment = "a".repeat(64);
        let started = std::time::Instant::now();

        assert!(!glob_match(pattern, &segment));
        assert!(glob_match(pattern, &format!("{segment}b")));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "matching must not backtrack exponentially"
        );

        let nested = "**/a/**/a/**/a/**/a/**/b";
        let path = "a/".repeat(24) + "c";
        let started = std::time::Instant::now();
        assert!(!matches_path(nested, &path));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "`**` must not backtrack exponentially either"
        );
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

    /// A backslash is a legal byte in a macOS filename, and the archive names
    /// paths for a Linux guest that reads it as one too. Rewriting it to `/`
    /// would turn an honest filename into a traversal the shim's own validation
    /// is then the only thing refusing, and would relocate innocent files into
    /// directories the context never had.
    #[test]
    fn a_backslash_in_a_filename_is_carried_not_rewritten() {
        let dir = tempfile::tempdir().expect("temp context");
        write(dir.path(), r"..\..\..\etc\passwd", "not a traversal");
        write(dir.path(), r"a\b.txt", "one file, not two");

        let (_, archive) =
            build_context_tar(dir.path(), &ContextFilter::default()).expect("context tar");

        let mut names = tar_names(&archive);
        names.sort();
        assert_eq!(
            names,
            vec![r"..\..\..\etc\passwd".to_string(), r"a\b.txt".to_string()],
            "the archive must name each file the way the filesystem does"
        );
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
