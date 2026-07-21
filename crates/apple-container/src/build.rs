use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

use crate::error::AppleContainerError;
use crate::models::{ContainerSnapshot, RuntimeStatus};
use crate::routes::{IMAGE_SERVICE_NAME, ImageRoute, XpcKey, XpcRoute};
use crate::xpc::connection::XpcConnection;
use crate::xpc::message::XpcMessage;
use crate::{content, fssync};

/// Content-store methods the builder proxies to the host.
const CONTENT_INFO_METHOD: &str = "/containerd.services.content.v1.Content/Info";
const CONTENT_READER_AT_METHOD: &str = "/containerd.services.content.v1.Content/ReaderAt";

/// How long to wait for the builder to send anything before giving up.
///
/// Every stall in this protocol is silent on both sides — the shim's receivers
/// have no deadline — so without this a protocol mismatch presents as an
/// indefinite hang rather than an error. The window is wide enough that a long
/// silent `RUN` step cannot trip it.
const BUILDER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Include the generated protobuf/gRPC code.
pub mod proto {
    tonic::include_proto!("com.apple.container.build.v1");
}

use proto::builder_client::BuilderClient;
use proto::{
    BuildTransfer, ClientStream, ImageTransfer, ServerStream, TransferDirection, client_stream,
    server_stream,
};

/// Container ID for the Apple Containers builder VM.
const BUILDER_CONTAINER_ID: &str = "buildkit";

/// Get DNS nameservers for the builder VM.
///
/// Reads the host's /etc/resolv.conf to find nameservers. Falls back to
/// well-known public DNS if none are found.
fn get_dns_nameservers() -> Vec<String> {
    if let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") {
        let servers: Vec<String> = contents
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with("nameserver") {
                    line.split_whitespace().nth(1).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !servers.is_empty() {
            return servers;
        }
    }
    vec!["8.8.8.8".to_string()]
}

/// Build an image using the Apple Containers builder VM.
///
/// 1. Ensure a builder VM is running.
/// 2. `containerDial` to get a vsock fd to the builder VM's gRPC port.
/// 3. Wrap the fd in a tokio stream and create a tonic gRPC channel.
/// 4. Call the Apple Builder gRPC service's `PerformBuild` RPC.
pub async fn build_image(
    conn: &XpcConnection,
    dockerfile: &str,
    context: &Path,
    tag: &str,
    no_cache: bool,
    verbose: bool,
) -> Result<(), AppleContainerError> {
    // Step 1: Ensure builder VM is running.
    ensure_builder(conn).await?;

    // Step 2: Connect via vsock (retry until the shim is listening).
    let mut client: Option<BuilderClient<tonic::transport::Channel>> = None;
    for attempt in 0..30 {
        let fd = match dial_container(conn, BUILDER_CONTAINER_ID, 8088).await {
            Ok(fd) => fd,
            Err(_) if attempt < 29 => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        let channel = match dial_builder_channel(fd).await {
            Ok(ch) => ch,
            Err(_) if attempt < 29 => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut c = BuilderClient::new(channel);
        // Verify the channel works with a simple unary call.
        match c.info(proto::InfoRequest {}).await {
            Ok(_resp) => {
                client = Some(c);
                break;
            }
            Err(e) => {
                if attempt == 29 {
                    return Err(AppleContainerError::XpcError(format!(
                        "builder gRPC server not ready after 30s: {e}"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    let _client = client.unwrap();

    // Step 3: Resolve context path.
    let abs_context = std::fs::canonicalize(context).map_err(AppleContainerError::Io)?;

    // Step 4: Build via PerformBuild bidirectional stream.
    let build_id = uuid::Uuid::new_v4().to_string();
    let context_str = abs_context.to_string_lossy().to_string();

    // The shim writes `<exports>/<build-id>/out.tar` but only creates that
    // directory itself for a `local` export (`pkg/build/build.go`), so an
    // `oci` export fails at the very last step unless the host creates it.
    let export = ExportDir::create(&build_id)?;

    // Dial a fresh connection for PerformBuild. The info() call completes
    // the HTTP/2 handshake — every successful test had this warmup.
    let dockerfile_b64 = base64_encode(dockerfile.as_bytes());

    let fd2 = dial_container(conn, BUILDER_CONTAINER_ID, 8088).await?;
    let ch2 = dial_builder_channel(fd2).await?;
    let mut build_client = BuilderClient::new(ch2);

    build_client
        .info(proto::InfoRequest {})
        .await
        .map_err(|e| AppleContainerError::XpcError(format!("fresh info() failed: {e}")))?;

    // All headers matching the Swift reference client.
    let (client_tx, client_rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
    let client_stream = tokio_stream::wrappers::ReceiverStream::new(client_rx);

    let mut request = tonic::Request::new(client_stream);
    let md = request.metadata_mut();
    md.insert("build-id", build_id.parse().unwrap());
    md.insert("tag", tag.parse().unwrap());
    md.insert("progress", "plain".parse().unwrap());
    md.insert("target", "".parse().unwrap());
    md.insert("context", context_str.parse().unwrap());
    md.insert("dockerfile", dockerfile_b64.parse().unwrap());
    // The Go server panics with "assignment to entry in nil map" if no outputs
    // header is sent — the default ExportEntry has a nil Attrs map. Sending
    // this forces the parseOutputCSV path which initialises the map properly.
    // `name` makes BuildKit annotate the exported layout with the tag, which
    // is what `imageLoad` registers the image under.
    md.insert(
        "outputs",
        format!("type=oci,name={tag}").parse().map_err(|_| {
            AppleContainerError::XpcError(format!("tag {tag:?} is not a valid header value"))
        })?,
    );
    if no_cache {
        md.insert("no-cache", "".parse().unwrap());
    }

    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        build_client.perform_build(request),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(AppleContainerError::XpcError(format!(
                "PerformBuild failed: {e}"
            )));
        }
        Err(_) => {
            return Err(AppleContainerError::XpcError(
                "PerformBuild timed out".to_string(),
            ));
        }
    };

    let mut server_stream = response.into_inner();

    // Process the bidirectional stream.
    process_build_stream(
        &mut server_stream,
        client_tx,
        &build_id,
        &abs_context,
        verbose,
    )
    .await?;

    // A finished stream only means BuildKit wrote its OCI layout; the image
    // does not exist to the daemon until it is loaded from that archive.
    register_built_image(&export.archive(), tag).await
}

/// The per-build directory the builder writes its export into.
///
/// Removed when the build ends so a failed or abandoned build cannot leave a
/// multi-megabyte archive behind.
struct ExportDir {
    path: std::path::PathBuf,
}

impl ExportDir {
    fn create(build_id: &str) -> Result<Self, AppleContainerError> {
        let path = builder_exports_root().join(build_id);
        std::fs::create_dir_all(&path).map_err(AppleContainerError::Io)?;
        Ok(Self { path })
    }

    fn archive(&self) -> std::path::PathBuf {
        self.path.join("out.tar")
    }
}

impl Drop for ExportDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Host directory mapped into the builder VM as its export target.
fn builder_exports_root() -> std::path::PathBuf {
    content::application_support_root().join("builder")
}

/// Import a finished build into the daemon's image store under `tag`.
///
/// Without this the build would report success for a tag that does not exist,
/// and creating a container from it would then fail.
async fn register_built_image(archive: &Path, tag: &str) -> Result<(), AppleContainerError> {
    if !archive.is_file() {
        return Err(AppleContainerError::XpcError(format!(
            "builder finished without writing an image archive to {}",
            archive.display()
        )));
    }

    let loaded = load_image_archive(archive).await?;
    if loaded.iter().any(|image| image.reference == tag) {
        return Ok(());
    }

    // The daemon names an image from its layout annotations and falls back to
    // `untagged@<digest>`, so an archive BuildKit did not annotate still needs
    // the tag applied.
    let source = loaded.first().ok_or_else(|| {
        AppleContainerError::XpcError(format!(
            "{} contained no image to register",
            archive.display()
        ))
    })?;
    tag_image(&source.reference, tag).await
}

/// Load an OCI layout archive into the daemon's image store.
pub async fn load_image_archive(
    archive: &Path,
) -> Result<Vec<crate::models::ImageDescription>, AppleContainerError> {
    let img_conn = XpcConnection::connect(IMAGE_SERVICE_NAME)?;

    let msg = XpcMessage::with_route(ImageRoute::ImageLoad.as_str());
    msg.set_string(XpcKey::FILE_PATH, &archive.to_string_lossy());
    msg.set_bool(XpcKey::FORCE_LOAD, false);

    let reply = img_conn.send_async(&msg).await?;
    reply.check_error()?;

    if let Some(raw) = reply.get_data(XpcKey::REJECTED_MEMBERS) {
        let rejected: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
        if !rejected.is_empty() {
            return Err(AppleContainerError::XpcError(format!(
                "image archive contained files the daemon refused: {}",
                rejected.join(", ")
            )));
        }
    }

    let data = reply.get_data(XpcKey::IMAGE_DESCRIPTIONS).ok_or_else(|| {
        AppleContainerError::XpcError("imageLoad reply missing imageDescriptions".to_string())
    })?;
    Ok(serde_json::from_slice(&data)?)
}

/// Point a new reference at an image already in the store.
pub async fn tag_image(reference: &str, new_reference: &str) -> Result<(), AppleContainerError> {
    let img_conn = XpcConnection::connect(IMAGE_SERVICE_NAME)?;

    let msg = XpcMessage::with_route(ImageRoute::ImageTag.as_str());
    msg.set_string(XpcKey::IMAGE_REFERENCE, reference);
    msg.set_string(XpcKey::IMAGE_NEW_REFERENCE, new_reference);

    let reply = img_conn.send_async(&msg).await?;
    reply.check_error()?;
    Ok(())
}

/// Process the PerformBuild bidirectional stream.
///
/// The server sends requests for files (BuildTransfer with fssync operations)
/// and build output (IO with stdout/stderr). We respond with file data.
async fn process_build_stream(
    server_stream: &mut tonic::Streaming<ServerStream>,
    client_tx: tokio::sync::mpsc::Sender<ClientStream>,
    _build_id: &str,
    context: &Path,
    verbose: bool,
) -> Result<(), AppleContainerError> {
    use tokio_stream::StreamExt;

    let mut session = BuildSession {
        context,
        resolved: None,
        pulled: Vec::new(),
    };

    loop {
        let next = tokio::time::timeout(BUILDER_IDLE_TIMEOUT, server_stream.next())
            .await
            .map_err(|_| {
                AppleContainerError::XpcError(format!(
                    "builder sent nothing for {}s; giving up on the build",
                    BUILDER_IDLE_TIMEOUT.as_secs()
                ))
            })?;
        let Some(msg) = next else { break };
        let msg =
            msg.map_err(|e| AppleContainerError::XpcError(format!("build stream error: {e}")))?;

        // CRITICAL: The server registers a demux handler keyed by
        // ServerStream.build_id (a per-request UUID, NOT the overall build ID).
        // Our responses must echo this value back as ClientStream.build_id
        // or the server drops the response with "no matching handler".
        let reply_id = &msg.build_id;

        match msg.packet_type {
            Some(server_stream::PacketType::Io(io)) => {
                handle_io(&io, verbose);
                // The Go StdioProxy.Write() calls Request() which blocks until
                // the client sends an ack response. Without this, the entire
                // build pipeline stalls (clog() never returns, resolver never runs).
                send_io_ack(&client_tx, reply_id).await?;
            }
            Some(server_stream::PacketType::BuildError(err)) => {
                return Err(AppleContainerError::XpcError(format!(
                    "Build failed: {}",
                    err.message
                )));
            }
            Some(server_stream::PacketType::CommandComplete(ref _cmd)) => {}
            Some(server_stream::PacketType::BuildTransfer(transfer)) => {
                handle_build_transfer(&transfer, &client_tx, reply_id, session.context).await?;
            }
            Some(server_stream::PacketType::ImageTransfer(ref transfer)) => {
                let stage = transfer
                    .metadata
                    .get("stage")
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let method = transfer
                    .metadata
                    .get("method")
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if stage == "resolver" && method == "/resolve" {
                    handle_image_resolve(transfer, &client_tx, reply_id, &mut session).await?;
                } else if stage == "content-store" {
                    handle_content_store(transfer, method, &client_tx, reply_id, &mut session)
                        .await?;
                }
            }
            None => {}
        }
    }

    Ok(())
}

/// State carried across one `PerformBuild` stream.
struct BuildSession<'a> {
    context: &'a Path,
    /// The base image most recently resolved, used to populate the local
    /// content store when the builder asks for a blob we do not have yet.
    resolved: Option<ResolvedImage>,
    /// References already pulled for this build, so a blob that is genuinely
    /// missing cannot send us pulling the same image over and over.
    pulled: Vec<String>,
}

/// A base image the builder asked us to resolve.
struct ResolvedImage {
    reference: String,
    platform: String,
}

/// Handle IO packets (stdout/stderr from the build).
fn handle_io(io: &proto::Io, verbose: bool) {
    use std::io::Write;
    match proto::Stdio::try_from(io.r#type) {
        Ok(proto::Stdio::Stdout) => {
            if verbose {
                let _ = std::io::stdout().write_all(&io.data);
            }
        }
        Ok(proto::Stdio::Stderr) => {
            let _ = std::io::stderr().write_all(&io.data);
        }
        _ => {}
    }
}

/// Send an IO ack response.
///
/// The Go builder shim's StdioProxy.Write() blocks until the client sends a
/// `Run` command containing a base64-encoded `{"command_type":"terminal","code":"ack"}`
/// JSON payload.  Without this ack the entire build pipeline deadlocks.
async fn send_io_ack(
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
) -> Result<(), AppleContainerError> {
    const ACK_JSON: &str = r#"{"command_type":"terminal","code":"ack","rows":0,"cols":0}"#;
    let ack_b64 = base64_encode(ACK_JSON.as_bytes())
        .trim_end_matches('=')
        .to_string();

    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::Command(proto::Run {
            id: build_id.to_string(),
            command: ack_b64,
        })),
    };

    client_tx
        .send(response)
        .await
        .map_err(|e| AppleContainerError::XpcError(format!("failed to send IO ack: {e}")))
}

/// Handle BuildTransfer packets — the server asking for file data (fssync).
///
/// The server sends BuildTransfer with metadata keys:
///   - "stage" = "fssync"
///   - "method" = "walk" | "read" | "info"
///
/// We respond with BuildTransfer packets containing the requested data.
async fn handle_build_transfer(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let stage = transfer
        .metadata
        .get("stage")
        .map(|s| s.as_str())
        .unwrap_or("");
    let method = transfer
        .metadata
        .get("method")
        .map(|s| s.as_str())
        .unwrap_or("");

    if stage != "fssync" {
        return Ok(());
    }

    // The builder shim sends capitalized method names (Walk, Read, Info).
    match method {
        "walk" | "Walk" => handle_walk(transfer, client_tx, build_id, context).await,
        "read" | "Read" => handle_read(transfer, client_tx, build_id, context).await,
        "info" | "Info" => handle_info(transfer, client_tx, build_id, context).await,
        _ => Ok(()),
    }
}

/// Metadata every fssync reply carries.
fn fssync_metadata(method: &str) -> HashMap<String, String> {
    HashMap::from([
        ("os".to_string(), "linux".to_string()),
        ("stage".to_string(), "fssync".to_string()),
        ("method".to_string(), method.to_string()),
    ])
}

/// Send one `BuildTransfer` reply on the request's id.
///
/// A failure to enqueue is fatal: the builder is waiting for this packet and
/// has no deadline of its own, so swallowing the error would hang the build.
async fn send_build_transfer(
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    transfer: &BuildTransfer,
    metadata: HashMap<String, String>,
    data: Vec<u8>,
    complete: bool,
    is_directory: bool,
) -> Result<(), AppleContainerError> {
    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::BuildTransfer(BuildTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Outof as i32,
            source: transfer.source.clone(),
            destination: None,
            data,
            complete,
            is_directory,
            metadata,
        })),
    };

    client_tx
        .send(response)
        .await
        .map_err(|e| AppleContainerError::XpcError(format!("failed to send fssync reply: {e}")))
}

/// Tell the builder a request failed instead of leaving it waiting.
///
/// Every shim receiver checks `metadata["error"]` first, so this turns what
/// would otherwise be an unbounded wait into a reported build failure.
async fn send_fssync_error(
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    transfer: &BuildTransfer,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    let mut metadata = fssync_metadata(method);
    metadata.insert("error".to_string(), message.to_string());
    send_build_transfer(
        client_tx,
        build_id,
        transfer,
        metadata,
        Vec::new(),
        true,
        false,
    )
    .await
}

/// Answer an fssync `Walk` by sending the build context as a tar archive.
///
/// The shim blocks in `readTarHash` until a packet carrying `hash` arrives and
/// only then starts draining the archive bytes, so the checksum goes first and
/// the tar follows in chunks (`pkg/fileutils/tarxfer.go`).
async fn handle_walk(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let filter = fssync::ContextFilter::from_metadata(&transfer.metadata);
    let built = fssync::require_tar_walk_mode(&transfer.metadata)
        .and_then(|()| fssync::build_context_tar(context, &filter));

    let (checksum, archive) = match built {
        Ok(built) => built,
        Err(e) => {
            send_fssync_error(client_tx, build_id, transfer, "Walk", &e.to_string()).await?;
            return Err(e);
        }
    };

    let mut hash_metadata = fssync_metadata("Walk");
    hash_metadata.insert("hash".to_string(), checksum);
    send_build_transfer(
        client_tx,
        build_id,
        transfer,
        hash_metadata,
        Vec::new(),
        false,
        false,
    )
    .await?;

    // `readTarHeader` blocks until at least one data packet arrives, so an
    // empty context still has to send its end-of-archive marker.
    let chunks: Vec<&[u8]> = if archive.is_empty() {
        vec![&[]]
    } else {
        archive.chunks(fssync::CONTEXT_CHUNK_SIZE).collect()
    };
    let last = chunks.len() - 1;
    for (index, chunk) in chunks.into_iter().enumerate() {
        send_build_transfer(
            client_tx,
            build_id,
            transfer,
            fssync_metadata("Walk"),
            chunk.to_vec(),
            index == last,
            false,
        )
        .await?;
    }

    Ok(())
}

/// Answer an fssync `Read` with a slice of a context file.
///
/// The shim sends the caller's buffer size as `length` (`pkg/fssync/file.go`)
/// and reads an empty reply as EOF.
async fn handle_read(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let source = transfer.source.as_deref().unwrap_or("");
    let path = resolve_path(context, source);
    let offset = numeric_metadata(&transfer.metadata, "offset").unwrap_or(0);
    let length = numeric_metadata(&transfer.metadata, "length").unwrap_or(0) as usize;

    let data = match content::read_range(&path, offset, length) {
        Ok(data) => data,
        Err(e) => {
            let message = format!("cannot read {source}: {e}");
            return send_fssync_error(client_tx, build_id, transfer, "Read", &message).await;
        }
    };

    let mut metadata = fssync_metadata("Read");
    metadata.insert("offset".to_string(), offset.to_string());
    metadata.insert("length".to_string(), data.len().to_string());
    send_build_transfer(client_tx, build_id, transfer, metadata, data, true, false).await
}

/// Answer an fssync `Info` with a context path's metadata.
///
/// The shim reads size, mode, timestamp and ownership out of the reply's
/// *metadata* map (`pkg/fileutils/file_info.go`); anything left out silently
/// becomes a zero, so a JSON body in `data` reads as an empty file.
async fn handle_info(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    use std::os::unix::fs::MetadataExt;

    let source = transfer.source.as_deref().unwrap_or("");
    let path = resolve_path(context, source);

    // `symlink_metadata` describes the link itself; BuildKit resolves links
    // on its own side and expects to be told the target.
    let file = match std::fs::symlink_metadata(&path) {
        Ok(file) => file,
        Err(e) => {
            // A missing path is routine — BuildKit probes for `.dockerignore`
            // on every build — so report it and let the builder decide.
            let message = format!("cannot stat {source}: {e}");
            return send_fssync_error(client_tx, build_id, transfer, "Info", &message).await;
        }
    };

    let mut metadata = fssync_metadata("Info");
    metadata.insert("size".to_string(), file.len().to_string());
    metadata.insert("mode".to_string(), fssync::go_file_mode(&file).to_string());
    metadata.insert(
        "modified_at".to_string(),
        fssync::rfc3339_utc(fssync::mtime_secs(&file)),
    );
    metadata.insert("uid".to_string(), file.uid().to_string());
    metadata.insert("gid".to_string(), file.gid().to_string());
    if file.is_symlink() {
        if let Ok(target) = std::fs::read_link(&path) {
            metadata.insert("target".to_string(), target.to_string_lossy().into_owned());
        }
    }

    send_build_transfer(
        client_tx,
        build_id,
        transfer,
        metadata,
        Vec::new(),
        true,
        file.is_dir(),
    )
    .await
}

/// Read a numeric metadata field the builder sent.
fn numeric_metadata(metadata: &HashMap<String, String>, key: &str) -> Option<u64> {
    metadata.get(key)?.trim().parse().ok()
}

/// Map the host's Rust architecture name to its OCI/Go equivalent.
fn host_oci_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64le",
        other => other,
    }
}

/// The platform to resolve base images for when the builder doesn't name one.
///
/// The builder VM always runs Linux, so only the architecture follows the
/// host; the OS must never be the host's (`darwin`).
fn default_build_platform() -> String {
    format!("linux/{}", host_oci_architecture())
}

/// Pick the platform to resolve a base image for from the builder's request,
/// falling back to [`default_build_platform`] when it names none.
fn requested_platform(metadata: &std::collections::HashMap<String, String>) -> String {
    metadata
        .get("platform")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_build_platform)
}

/// Split an OCI platform string (`os/arch`, optionally `os/arch/variant`)
/// into its os and architecture components.
fn split_platform(platform: &str) -> (String, String) {
    let mut parts = platform.split('/');
    let os = parts.next().unwrap_or_default().to_string();
    let arch = parts.next().unwrap_or_default().to_string();
    (os, arch)
}

/// Build an image-index resolver that selects the entry matching `platform`.
///
/// `oci_client`'s default resolver matches the *running* platform, which on
/// macOS is `darwin/<arch>`. No Linux image index contains such an entry, so
/// every multi-arch base image fails with "no entry found in image index
/// manifest matching client's default platform". The builder always wants a
/// Linux image, so resolve against the platform it requested instead.
///
/// Variants are ignored, matching `oci_client`'s own resolvers: an index
/// distinguishes `arm64` from `arm`, not `arm64/v8` from bare `arm64`.
fn platform_resolver(
    platform: &str,
) -> Box<dyn Fn(&[oci_client::manifest::ImageIndexEntry]) -> Option<String> + Send + Sync> {
    let (os, arch) = split_platform(platform);
    Box::new(move |manifests| {
        manifests
            .iter()
            .find(|entry| {
                entry
                    .platform
                    .as_ref()
                    .is_some_and(|p| p.os == os && p.architecture == arch)
            })
            .map(|entry| entry.digest.clone())
    })
}

/// Handle an image resolve request from the builder.
///
/// The server sends an `ImageTransfer` with `stage: "resolver"` and
/// `method: "/resolve"`. We pull the OCI image manifest and config from
/// the registry on the host side (fast) and send the config back so the
/// builder doesn't have to pull through the slow vsock network.
async fn handle_image_resolve(
    transfer: &ImageTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    session: &mut BuildSession<'_>,
) -> Result<(), AppleContainerError> {
    // The image reference is in metadata "ref" or the tag field.
    let reference = transfer
        .metadata
        .get("ref")
        .or_else(|| {
            if transfer.tag.is_empty() {
                None
            } else {
                Some(&transfer.tag)
            }
        })
        .ok_or_else(|| AppleContainerError::XpcError("image resolve: missing ref".into()))?;
    let platform_str = requested_platform(&transfer.metadata);

    let oci_ref: oci_client::Reference =
        reference.parse().map_err(|e: oci_client::ParseError| {
            AppleContainerError::XpcError(format!("invalid image ref: {e}"))
        })?;

    let client = oci_client::Client::new(oci_client::client::ClientConfig {
        platform_resolver: Some(platform_resolver(&platform_str)),
        ..Default::default()
    });
    let auth = oci_client::secrets::RegistryAuth::Anonymous;
    client
        .auth(&oci_ref, &auth, oci_client::RegistryOperation::Pull)
        .await
        .map_err(|e| AppleContainerError::XpcError(format!("registry auth failed: {e}")))?;

    let (manifest, digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| {
            AppleContainerError::XpcError(format!("failed to pull manifest for {reference}: {e}"))
        })?;

    let mut config_data = Vec::new();
    client
        .pull_blob(&oci_ref, manifest.config.digest.as_str(), &mut config_data)
        .await
        .map_err(|e| {
            AppleContainerError::XpcError(format!("failed to pull config for {reference}: {e}"))
        })?;

    // Remember what this build is based on: if the builder later asks for a
    // blob the host does not have, this is the image that supplies it.
    session.resolved = Some(ResolvedImage {
        reference: reference.clone(),
        platform: platform_str.clone(),
    });

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("os".to_string(), "linux".to_string());
    metadata.insert("stage".to_string(), "resolver".to_string());
    metadata.insert("method".to_string(), "/resolve".to_string());
    metadata.insert("ref".to_string(), reference.clone());
    metadata.insert("platform".to_string(), platform_str.to_string());

    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::ImageTransfer(ImageTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Into as i32,
            tag: digest.clone(),
            descriptor: None,
            data: config_data,
            complete: true,
            metadata,
        })),
    };
    let _ = client_tx.send(response).await;
    Ok(())
}

/// Handle content-store requests from the builder (BuildRemoteContentProxy).
///
/// The builder sends `ImageTransfer` with `stage: "content-store"` when it
/// needs to read content (blobs/layers) from the host. Supported methods:
///   - `/containerd.services.content.v1.Content/Info` — get content size
///   - `/containerd.services.content.v1.Content/ReaderAt` — read content bytes
///
/// We pull the requested blob from the OCI registry on the host side and
/// serve it back to the builder.
async fn handle_content_store(
    transfer: &ImageTransfer,
    method: &str,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    session: &mut BuildSession<'_>,
) -> Result<(), AppleContainerError> {
    if !matches!(method, CONTENT_INFO_METHOD | CONTENT_READER_AT_METHOD) {
        return Ok(());
    }

    let Some(digest) = content_digest(transfer) else {
        return send_content_error(
            client_tx,
            build_id,
            transfer,
            method,
            "content-store request named no digest",
        )
        .await;
    };

    let size = match ensure_blob(&digest, session).await {
        Some(size) => size,
        None => {
            let message = format!("blob {digest} is not in the local content store");
            return send_content_error(client_tx, build_id, transfer, method, &message).await;
        }
    };

    // `ReaderAt` probes with offset 0 and length 0 purely to learn the size,
    // and reads an empty payload as EOF thereafter.
    let offset = numeric_metadata(&transfer.metadata, "offset").unwrap_or(0);
    let length = numeric_metadata(&transfer.metadata, "length").unwrap_or(0) as usize;
    let data = if method == CONTENT_READER_AT_METHOD && length > 0 {
        match content::read_blob_range(&digest, offset, length) {
            Ok(data) => data,
            Err(e) => {
                let message = format!("cannot read blob {digest}: {e}");
                return send_content_error(client_tx, build_id, transfer, method, &message).await;
            }
        }
    } else {
        Vec::new()
    };

    let mut metadata = content_store_metadata(method);
    metadata.insert("size".to_string(), size.to_string());
    metadata.insert("offset".to_string(), offset.to_string());
    metadata.insert("length".to_string(), data.len().to_string());
    // Both timestamps are parsed as RFC 3339; the store keeps no creation
    // time of its own, so report the blob's mtime for both.
    let stored_at = blob_timestamp(&digest);
    metadata.insert("created_at".to_string(), stored_at.clone());
    metadata.insert("updated_at".to_string(), stored_at);

    send_image_transfer(client_tx, build_id, transfer, &digest, metadata, data).await
}

/// Metadata every content-store reply carries.
fn content_store_metadata(method: &str) -> HashMap<String, String> {
    HashMap::from([
        ("os".to_string(), "linux".to_string()),
        ("stage".to_string(), "content-store".to_string()),
        ("method".to_string(), method.to_string()),
    ])
}

/// The digest a content-store request refers to.
///
/// `Info` puts it in `tag`; `ReaderAt` sends a descriptor instead.
fn content_digest(transfer: &ImageTransfer) -> Option<String> {
    if !transfer.tag.is_empty() {
        return Some(transfer.tag.clone());
    }
    transfer
        .descriptor
        .as_ref()
        .map(|d| d.digest.clone())
        .filter(|digest| !digest.is_empty())
}

/// Size of a blob, pulling the build's base image first if it is missing.
///
/// The builder only asks the host for content it cannot find in its own cache,
/// which happens for a base image the daemon has never unpacked. Pulling the
/// resolved reference populates the daemon's content store, and the pull is
/// attempted once per reference so a genuinely absent blob fails instead of
/// looping.
async fn ensure_blob(digest: &str, session: &mut BuildSession<'_>) -> Option<u64> {
    if let Some(size) = content::blob_size(digest) {
        return Some(size);
    }

    let resolved = session.resolved.as_ref()?;
    if session.pulled.iter().any(|r| r == &resolved.reference) {
        return None;
    }
    let (reference, platform) = (resolved.reference.clone(), resolved.platform.clone());
    session.pulled.push(reference.clone());

    let (os, architecture) = split_platform(&platform);
    let platform_json = serde_json::to_vec(&serde_json::json!({
        "os": os,
        "architecture": architecture,
    }))
    .ok()?;
    pull_image(&reference, &platform_json).await.ok()?;

    content::blob_size(digest)
}

/// When a blob was last written, formatted the way the shim parses it.
fn blob_timestamp(digest: &str) -> String {
    let stored = content::blob_path(digest)
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| fssync::mtime_secs(&meta))
        .unwrap_or(0);
    fssync::rfc3339_utc(stored)
}

/// Send one content-store reply.
async fn send_image_transfer(
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    transfer: &ImageTransfer,
    digest: &str,
    metadata: HashMap<String, String>,
    data: Vec<u8>,
) -> Result<(), AppleContainerError> {
    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::ImageTransfer(ImageTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Into as i32,
            tag: digest.to_string(),
            descriptor: transfer.descriptor.clone(),
            data,
            complete: true,
            metadata,
        })),
    };

    client_tx.send(response).await.map_err(|e| {
        AppleContainerError::XpcError(format!("failed to send content-store reply: {e}"))
    })
}

/// Report a content-store failure rather than leaving the builder waiting.
async fn send_content_error(
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    transfer: &ImageTransfer,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    let mut metadata = content_store_metadata(method);
    metadata.insert("error".to_string(), message.to_string());
    send_image_transfer(
        client_tx,
        build_id,
        transfer,
        &transfer.tag.clone(),
        metadata,
        Vec::new(),
    )
    .await
}

/// Resolve a source path relative to the context directory.
fn resolve_path(context: &Path, source: &str) -> std::path::PathBuf {
    if source.starts_with('/') {
        std::path::PathBuf::from(source)
    } else {
        context.join(source)
    }
}

/// Simple base64 encoding (no padding).
fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut encoder = Base64Encoder::new(&mut buf);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
    }
    String::from_utf8(buf).unwrap()
}

/// Minimal base64 encoder.
struct Base64Encoder<W: std::io::Write> {
    writer: W,
    buf: [u8; 3],
    len: usize,
}

const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl<W: std::io::Write> Base64Encoder<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            buf: [0; 3],
            len: 0,
        }
    }

    fn flush_buf(&mut self) -> std::io::Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let b = &self.buf;
        let mut out = [b'='; 4];
        out[0] = B64_CHARS[(b[0] >> 2) as usize];
        out[1] = B64_CHARS[((b[0] & 0x03) << 4 | b[1] >> 4) as usize];
        if self.len > 1 {
            out[2] = B64_CHARS[((b[1] & 0x0f) << 2 | b[2] >> 6) as usize];
        }
        if self.len > 2 {
            out[3] = B64_CHARS[(b[2] & 0x3f) as usize];
        }
        self.writer.write_all(&out)?;
        self.buf = [0; 3];
        self.len = 0;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<W> {
        self.flush_buf()?;
        Ok(self.writer)
    }
}

impl<W: std::io::Write> std::io::Write for Base64Encoder<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut written = 0;
        for &byte in data {
            self.buf[self.len] = byte;
            self.len += 1;
            if self.len == 3 {
                self.flush_buf()?;
            }
            written += 1;
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// Get a vsock file descriptor to a container on a given port via XPC `containerDial`.
pub async fn dial_container(
    conn: &XpcConnection,
    container_id: &str,
    port: u32,
) -> Result<std::os::fd::RawFd, AppleContainerError> {
    let msg = XpcMessage::with_route(XpcRoute::ContainerDial.as_str());
    msg.set_string(XpcKey::ID, container_id);
    msg.set_uint64(XpcKey::PORT, port as u64);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;

    reply
        .dup_fd(XpcKey::FD)
        .ok_or_else(|| AppleContainerError::XpcError("containerDial reply missing fd".to_string()))
}

/// Create a tonic gRPC channel from a vsock file descriptor.
async fn dial_builder_channel(
    fd: std::os::fd::RawFd,
) -> Result<tonic::transport::Channel, AppleContainerError> {
    // Set socket buffer sizes to match the Swift builder client.
    unsafe {
        let send_buf: libc::c_int = 4 << 20; // 4 MB
        let recv_buf: libc::c_int = 2 << 20; // 2 MB
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &send_buf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &recv_buf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream
        .set_nonblocking(true)
        .map_err(AppleContainerError::Io)?;
    let tokio_stream =
        tokio::net::UnixStream::from_std(std_stream).map_err(AppleContainerError::Io)?;

    let stream_slot = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tokio_stream)));
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
        .map_err(|e| AppleContainerError::ConnectionFailed(format!("tonic endpoint: {e}")))?
        .initial_stream_window_size(16 << 10) // 16 KB — match Swift httpTargetWindowSize
        .initial_connection_window_size(16 << 10)
        .http2_max_header_list_size(512 * 1024) // allow large header blocks (dockerfile)
        .http2_keep_alive_interval(std::time::Duration::from_secs(600))
        .keep_alive_timeout(std::time::Duration::from_secs(500))
        .connect_with_connector(tower::service_fn({
            let stream_slot = stream_slot.clone();
            let call_count = call_count.clone();
            move |_: tonic::transport::Uri| {
                let stream_slot = stream_slot.clone();
                let call_count = call_count.clone();
                async move {
                    let _n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let stream = stream_slot.lock().await.take().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::Other, "stream already consumed")
                    })?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }
        }))
        .await
        .map_err(|e| AppleContainerError::ConnectionFailed(format!("builder connect: {e}")))?;

    Ok(channel)
}

/// Pull an image via the image service XPC, returning the ImageDescription JSON bytes.
///
/// The returned bytes contain `{"reference":"...","descriptor":{"mediaType":"...","digest":"...","size":...}}`.
pub async fn pull_image(
    reference: &str,
    platform_json: &[u8],
) -> Result<Vec<u8>, AppleContainerError> {
    let img_conn = XpcConnection::connect(IMAGE_SERVICE_NAME)?;

    let msg = XpcMessage::with_route(ImageRoute::ImagePull.as_str());
    msg.set_string(XpcKey::IMAGE_REFERENCE, reference);
    msg.set_data(XpcKey::OCI_PLATFORM, platform_json);
    msg.set_bool(XpcKey::INSECURE_FLAG, false);
    msg.set_int64(XpcKey::MAX_CONCURRENT_DOWNLOADS, 3);

    let reply = img_conn.send_async(&msg).await?;
    reply.check_error()?;

    reply.get_data(XpcKey::IMAGE_DESCRIPTION).ok_or_else(|| {
        AppleContainerError::XpcError("imagePull reply missing imageDescription".to_string())
    })
}

/// Unpack an image snapshot via the image service XPC.
pub async fn unpack_image(
    image_desc: &[u8],
    platform_json: &[u8],
) -> Result<(), AppleContainerError> {
    let img_conn = XpcConnection::connect(IMAGE_SERVICE_NAME)?;

    let msg = XpcMessage::with_route(ImageRoute::ImageUnpack.as_str());
    msg.set_data(XpcKey::IMAGE_DESCRIPTION, image_desc);
    msg.set_data(XpcKey::OCI_PLATFORM, platform_json);

    let reply = img_conn.send_async(&msg).await?;
    reply.check_error()?;
    Ok(())
}

/// Fetch the default kernel from the daemon via the `getDefaultKernel` XPC route.
///
/// Returns the raw JSON bytes of the `Kernel` struct, which can be passed
/// directly to `containerCreate` without deserialization.
async fn get_default_kernel(conn: &XpcConnection) -> Result<Vec<u8>, AppleContainerError> {
    let msg = XpcMessage::with_route(XpcRoute::GetDefaultKernel.as_str());

    let platform_json = serde_json::to_vec(&serde_json::json!({
        "os": "linux",
        "architecture": "arm64"
    }))?;
    msg.set_data(XpcKey::SYSTEM_PLATFORM, &platform_json);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;

    reply.get_data(XpcKey::KERNEL).ok_or_else(|| {
        AppleContainerError::XpcError("getDefaultKernel reply missing kernel data".to_string())
    })
}

/// Builder OCI image reference.
const BUILDER_IMAGE: &str = "ghcr.io/apple/container-builder-shim/builder:0.8.0";

/// Ensure the builder VM exists and is running via XPC.
pub async fn ensure_builder(conn: &XpcConnection) -> Result<(), AppleContainerError> {
    // Step 1: Check if the builder container already exists.
    let snapshot = get_container(conn, BUILDER_CONTAINER_ID).await;

    match snapshot {
        Some(snap) if snap.status == RuntimeStatus::Running => {
            return Ok(());
        }
        Some(snap) if snap.status == RuntimeStatus::Stopped => {
            bootstrap_container(conn, BUILDER_CONTAINER_ID).await?;
            start_process(conn, BUILDER_CONTAINER_ID).await?;
            wait_for_running(conn, BUILDER_CONTAINER_ID).await?;
            return Ok(());
        }
        Some(_) => {
            // Unknown/Stopping — try bootstrap anyway.
            bootstrap_container(conn, BUILDER_CONTAINER_ID).await?;
            start_process(conn, BUILDER_CONTAINER_ID).await?;
            wait_for_running(conn, BUILDER_CONTAINER_ID).await?;
            return Ok(());
        }
        None => {
            // Not found — create it.
        }
    }

    let platform_json = serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "variant": "v8"
    }))?;

    let image_desc_bytes = pull_image(BUILDER_IMAGE, &platform_json).await?;
    let image_desc: serde_json::Value = serde_json::from_slice(&image_desc_bytes)?;

    unpack_image(&image_desc_bytes, &platform_json).await?;

    let kernel_bytes = get_default_kernel(conn).await?;

    // Ensure the exports directory exists (the builder shim writes build
    // outputs here via virtiofs).
    let exports_dir = builder_exports_root();
    std::fs::create_dir_all(&exports_dir).map_err(AppleContainerError::Io)?;

    // Build the full config as raw JSON — the builder needs fields
    // (networks, mount types, rosetta) that our ContainerConfiguration
    // model doesn't cover.
    let config_json = serde_json::json!({
        "id": BUILDER_CONTAINER_ID,
        "image": image_desc,
        "initProcess": {
            "executable": "/usr/local/bin/container-builder-shim",
            "arguments": ["--debug", "--vsock"],
            "environment": [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "BUILDKIT_SETUP_CGROUPV2_ROOT=1"
            ],
            "workingDirectory": "/",
            "terminal": false,
            "user": { "id": { "uid": 0, "gid": 0 } },
            "supplementalGroups": [],
            "rlimits": []
        },
        "resources": {
            "cpus": 2,
            "memoryInBytes": 2_147_483_648_u64
        },
        "networks": [
            { "network": "default", "options": { "hostname": "buildkit" } }
        ],
        "mounts": [
            { "type": { "tmpfs": {} }, "source": "", "destination": "/run", "options": [] },
            {
                "type": { "virtiofs": {} },
                "source": exports_dir.to_string_lossy(),
                "destination": "/var/lib/container-builder-shim/exports",
                "options": []
            }
        ],
        "labels": {
            "com.apple.container.resource.role": "builder"
        },
        "dns": {
            "nameservers": get_dns_nameservers(),
            "domain": null,
            "searchDomains": [],
            "options": []
        },
        "rosetta": true,
        "publishedPorts": [],
        "publishedSockets": []
    });

    let msg = XpcMessage::with_route(XpcRoute::ContainerCreate.as_str());
    let config_bytes = serde_json::to_vec(&config_json)?;
    msg.set_data(XpcKey::CONTAINER_CONFIG, &config_bytes);
    msg.set_data(XpcKey::KERNEL, &kernel_bytes);
    let options_bytes = serde_json::to_vec(&serde_json::json!({"autoRemove": false}))?;
    msg.set_data(XpcKey::CONTAINER_OPTIONS, &options_bytes);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;

    // Drop create request/reply before the long polling loop.
    drop(reply);
    drop(msg);

    bootstrap_container(conn, BUILDER_CONTAINER_ID).await?;

    start_process(conn, BUILDER_CONTAINER_ID).await?;

    wait_for_running(conn, BUILDER_CONTAINER_ID).await?;

    Ok(())
}

/// Get a container snapshot by ID, returning `None` if not found.
///
/// Uses `containerList` and filters by ID, since the list route returns
/// snapshot data under a well-known key (`containers`).
async fn get_container(conn: &XpcConnection, id: &str) -> Option<ContainerSnapshot> {
    let msg = XpcMessage::with_route(XpcRoute::ContainerList.as_str());

    let reply = conn.send_async(&msg).await.ok()?;
    if reply.check_error().is_err() {
        return None;
    }

    let data = reply.get_data(XpcKey::CONTAINERS)?;
    let snapshots: Vec<ContainerSnapshot> = serde_json::from_slice(&data).ok()?;
    snapshots.into_iter().find(|s| s.configuration.id == id)
}

/// Bootstrap a container with /dev/null stdio fds.
async fn bootstrap_container(conn: &XpcConnection, id: &str) -> Result<(), AppleContainerError> {
    let devnull = std::fs::File::open("/dev/null").map_err(AppleContainerError::Io)?;
    let fd = devnull.as_raw_fd();

    let msg = XpcMessage::with_route(XpcRoute::ContainerBootstrap.as_str());
    msg.set_string(XpcKey::ID, id);
    msg.set_fd(XpcKey::STDIN, fd);
    msg.set_fd(XpcKey::STDOUT, fd);
    msg.set_fd(XpcKey::STDERR, fd);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;
    Ok(())
}

/// Start the init process inside a bootstrapped container.
async fn start_process(conn: &XpcConnection, id: &str) -> Result<(), AppleContainerError> {
    let msg = XpcMessage::with_route(XpcRoute::ContainerStartProcess.as_str());
    msg.set_string(XpcKey::ID, id);
    msg.set_string(XpcKey::PROCESS_IDENTIFIER, id);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;
    Ok(())
}

/// Poll until the container reaches Running status (up to ~30 seconds).
async fn wait_for_running(conn: &XpcConnection, id: &str) -> Result<(), AppleContainerError> {
    for _ in 0..30 {
        if let Some(snap) = get_container(conn, id).await {
            if snap.status == RuntimeStatus::Running {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err(AppleContainerError::XpcError(
        "Builder VM did not reach Running state within 30 seconds".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_client::manifest::ImageIndexEntry;

    /// Run one handler to completion on a throwaway runtime.
    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    const REPLY_ID: &str = "per-request-demux-id";

    fn build_transfer(metadata: &[(&str, &str)], source: &str) -> BuildTransfer {
        BuildTransfer {
            id: "request-1".to_string(),
            direction: TransferDirection::Outof as i32,
            source: Some(source.to_string()),
            destination: None,
            data: Vec::new(),
            complete: false,
            is_directory: false,
            metadata: metadata
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Collect the `BuildTransfer` replies a handler queued.
    fn drain(rx: &mut tokio::sync::mpsc::Receiver<ClientStream>) -> Vec<(String, BuildTransfer)> {
        rx.close();
        let mut replies = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Some(client_stream::PacketType::BuildTransfer(transfer)) = msg.packet_type {
                replies.push((msg.build_id, transfer));
            }
        }
        replies
    }

    fn context_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp context");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("context subdirectory");
            }
            std::fs::write(path, contents).expect("context file");
        }
        dir
    }

    /// The exact shape issue #4's hang came down to: the shim blocks in
    /// `readTarHash` until a packet carrying `hash` arrives, and only then
    /// drains the archive. Answering in `json` mode sent neither.
    #[test]
    fn walk_replies_with_a_checksum_packet_and_then_the_archive() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("stage", "fssync"), ("method", "Walk"), ("mode", "tar")], ".");

        run(handle_walk(&request, &tx, REPLY_ID, dir.path())).expect("a tar walk must succeed");
        let replies = drain(&mut rx);

        assert!(replies.len() >= 2, "expected a hash packet and an archive");
        let (_, hash_packet) = &replies[0];
        assert!(
            hash_packet.metadata.contains_key("hash"),
            "the first packet must carry the checksum: {:?}",
            hash_packet.metadata
        );
        assert!(hash_packet.data.is_empty(), "the hash packet carries no data");
        assert!(!hash_packet.complete);

        assert!(
            replies.iter().all(|(_, p)| p.metadata.get("mode").is_none()),
            "no reply may advertise a transfer mode the shim does not implement"
        );
        assert!(
            replies[1..].iter().all(|(_, p)| !p.metadata.contains_key("hash")),
            "only the first packet may carry a hash, or the rest are read as more hashes"
        );

        let (_, last) = replies.last().expect("at least one packet");
        assert!(last.complete, "the final archive packet must set `complete`");
        assert_eq!(
            replies[..replies.len() - 1]
                .iter()
                .filter(|(_, p)| p.complete)
                .count(),
            0,
            "`complete` ends the transfer, so only the last packet may set it"
        );

        // The archive packets concatenate into the tar the shim unpacks.
        let archive: Vec<u8> = replies[1..]
            .iter()
            .flat_map(|(_, p)| p.data.clone())
            .collect();
        let names: Vec<String> = tar::Archive::new(archive.as_slice())
            .entries()
            .expect("entries")
            .map(|e| e.expect("entry").path().expect("path").to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["app.txt".to_string()]);
    }

    /// Replies are demultiplexed by the per-request id, not the build's own.
    #[test]
    fn walk_replies_echo_the_requests_routing_ids() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        run(handle_walk(&request, &tx, REPLY_ID, dir.path())).expect("walk");

        for (build_id, transfer) in drain(&mut rx) {
            assert_eq!(build_id, REPLY_ID);
            assert_eq!(transfer.id, request.id);
        }
    }

    /// An unimplementable mode must fail loudly. Previously every request was
    /// answered in `json`, which deadlocked both sides silently.
    #[test]
    fn walk_reports_an_error_for_a_mode_the_shim_cannot_receive() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "json")], ".");

        let outcome = run(handle_walk(&request, &tx, REPLY_ID, dir.path()));
        assert!(outcome.is_err(), "an unsupported mode must not report success");

        let replies = drain(&mut rx);
        assert_eq!(replies.len(), 1, "exactly one error packet");
        let (_, error) = &replies[0];
        assert!(
            error.metadata.contains_key("error"),
            "the shim only stops waiting when it sees an `error` key: {:?}",
            error.metadata
        );
        assert!(error.complete);
    }

    /// An empty context still has to send a data packet: `readTarHeader`
    /// blocks until one arrives.
    #[test]
    fn walk_sends_an_archive_even_for_an_empty_context() {
        let dir = tempfile::tempdir().expect("temp context");
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        run(handle_walk(&request, &tx, REPLY_ID, dir.path())).expect("walk");
        let replies = drain(&mut rx);

        assert!(replies.len() >= 2, "a hash packet and at least one data packet");
        assert!(replies.last().expect("last").1.complete);
    }

    /// `pkg/fileutils/file_info.go` reads these out of the metadata map and
    /// silently substitutes zero for anything absent, so a JSON body in `data`
    /// made every file look empty.
    #[test]
    fn info_answers_in_metadata_rather_than_a_json_body() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "app.txt");

        run(handle_info(&request, &tx, REPLY_ID, dir.path())).expect("info");
        let replies = drain(&mut rx);

        let (_, reply) = &replies[0];
        assert!(reply.data.is_empty(), "the payload belongs in metadata");
        assert_eq!(reply.metadata.get("size").map(String::as_str), Some("7"));
        assert!(!reply.is_directory);
        for key in ["mode", "uid", "gid", "modified_at"] {
            assert!(reply.metadata.contains_key(key), "missing {key}");
        }
        // Parsed with `time.Parse(time.RFC3339, ...)`, which rejects an integer.
        let modified = reply.metadata.get("modified_at").expect("modified_at");
        assert!(modified.ends_with('Z') && modified.contains('T'), "{modified}");
    }

    #[test]
    fn info_flags_directories_so_the_walk_can_recurse() {
        let dir = context_with(&[("src/app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "src");

        run(handle_info(&request, &tx, REPLY_ID, dir.path())).expect("info");
        assert!(drain(&mut rx)[0].1.is_directory);
    }

    /// BuildKit probes for `.dockerignore` on every build; a missing path has
    /// to come back as an error rather than silently as an empty file.
    #[test]
    fn info_reports_a_missing_path_instead_of_pretending_it_is_empty() {
        let dir = tempfile::tempdir().expect("temp context");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], ".dockerignore");

        run(handle_info(&request, &tx, REPLY_ID, dir.path())).expect("info");
        assert!(drain(&mut rx)[0].1.metadata.contains_key("error"));
    }

    /// The shim sends the caller's buffer size as `length`; reading `len`
    /// meant every read returned the whole file from the offset.
    #[test]
    fn read_honours_the_length_the_shim_asked_for() {
        let dir = context_with(&[("app.txt", "0123456789")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(
            &[("method", "Read"), ("offset", "2"), ("length", "3")],
            "app.txt",
        );

        run(handle_read(&request, &tx, REPLY_ID, dir.path())).expect("read");
        assert_eq!(drain(&mut rx)[0].1.data, b"234");
    }

    #[test]
    fn read_past_the_end_of_a_file_comes_back_empty() {
        let dir = context_with(&[("app.txt", "0123456789")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(
            &[("method", "Read"), ("offset", "99"), ("length", "8")],
            "app.txt",
        );

        run(handle_read(&request, &tx, REPLY_ID, dir.path())).expect("read");
        assert!(
            drain(&mut rx)[0].1.data.is_empty(),
            "the shim reads an empty payload as EOF"
        );
    }

    /// A multi-arch index shaped like `docker.io/library/alpine:latest`:
    /// several Linux architectures plus an attestation entry carrying the
    /// `unknown/unknown` platform that registries attach to modern images.
    fn multi_arch_index() -> Vec<ImageIndexEntry> {
        serde_json::from_str(
            r#"[
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:amd64digest",
                    "size": 1,
                    "platform": {"architecture": "amd64", "os": "linux"}
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:armv7digest",
                    "size": 1,
                    "platform": {"architecture": "arm", "os": "linux", "variant": "v7"}
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:arm64digest",
                    "size": 1,
                    "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:attestationdigest",
                    "size": 1,
                    "platform": {"architecture": "unknown", "os": "unknown"}
                }
            ]"#,
        )
        .expect("index fixture must deserialize")
    }

    fn metadata(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn default_build_platform_targets_linux_not_the_host_os() {
        let platform = default_build_platform();
        assert!(
            platform.starts_with("linux/"),
            "builder images are Linux images; got {platform}"
        );
        assert_eq!(platform, format!("linux/{}", host_oci_architecture()));
    }

    #[test]
    fn host_architecture_uses_oci_names() {
        // The resolver compares against OCI/Go names, not Rust's.
        let arch = host_oci_architecture();
        assert_ne!(arch, "aarch64");
        assert_ne!(arch, "x86_64");
    }

    /// The regression this guards: `oci_client::Client::default()` resolves
    /// against the *running* platform, which on macOS is `darwin/<arch>`. No
    /// Linux index has such an entry, so every base image failed with
    /// "no entry found in image index manifest matching client's default
    /// platform" and `dev up --runtime apple` could not build its features
    /// image.
    #[cfg(target_os = "macos")]
    #[test]
    fn host_platform_resolver_cannot_resolve_a_linux_index() {
        let index = multi_arch_index();
        assert_eq!(
            oci_client::client::current_platform_resolver(&index),
            None,
            "the default resolver must not be used for builder image resolution"
        );
        assert!(
            platform_resolver(&default_build_platform())(&index).is_some(),
            "our default must resolve where the host-platform resolver cannot"
        );
    }

    #[test]
    fn resolver_selects_the_requested_architecture() {
        let index = multi_arch_index();
        assert_eq!(
            platform_resolver("linux/arm64")(&index).as_deref(),
            Some("sha256:arm64digest")
        );
        assert_eq!(
            platform_resolver("linux/amd64")(&index).as_deref(),
            Some("sha256:amd64digest")
        );
    }

    #[test]
    fn resolver_ignores_a_variant_suffix() {
        assert_eq!(
            platform_resolver("linux/arm64/v8")(&multi_arch_index()).as_deref(),
            Some("sha256:arm64digest")
        );
    }

    #[test]
    fn resolver_returns_none_when_no_entry_matches() {
        let index = multi_arch_index();
        assert_eq!(platform_resolver("linux/s390x")(&index), None);
        // The host OS must never match a Linux index.
        assert_eq!(platform_resolver("darwin/arm64")(&index), None);
    }

    #[test]
    fn resolver_skips_entries_without_platform_metadata() {
        let index: Vec<ImageIndexEntry> = serde_json::from_str(
            r#"[
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:nolatform",
                    "size": 1
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:arm64digest",
                    "size": 1,
                    "platform": {"architecture": "arm64", "os": "linux"}
                }
            ]"#,
        )
        .expect("index fixture must deserialize");
        assert_eq!(
            platform_resolver("linux/arm64")(&index).as_deref(),
            Some("sha256:arm64digest")
        );
    }

    #[test]
    fn requested_platform_honors_the_builders_request() {
        assert_eq!(
            requested_platform(&metadata(&[("platform", "linux/amd64")])),
            "linux/amd64"
        );
    }

    #[test]
    fn requested_platform_falls_back_when_absent_or_blank() {
        let expected = default_build_platform();
        assert_eq!(requested_platform(&metadata(&[])), expected);
        assert_eq!(requested_platform(&metadata(&[("platform", "")])), expected);
        assert_eq!(
            requested_platform(&metadata(&[("platform", "  ")])),
            expected
        );
    }

    #[test]
    fn split_platform_handles_os_arch_and_variant() {
        assert_eq!(
            split_platform("linux/arm64"),
            ("linux".to_string(), "arm64".to_string())
        );
        assert_eq!(
            split_platform("linux/arm64/v8"),
            ("linux".to_string(), "arm64".to_string())
        );
        assert_eq!(
            split_platform("linux"),
            ("linux".to_string(), String::new())
        );
    }
}
