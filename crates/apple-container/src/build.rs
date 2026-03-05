use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

use crate::error::AppleContainerError;
use crate::models::{ContainerSnapshot, RuntimeStatus};
use crate::routes::{ImageRoute, XpcKey, XpcRoute, IMAGE_SERVICE_NAME};
use crate::xpc::connection::XpcConnection;
use crate::xpc::message::XpcMessage;

/// Include the generated protobuf/gRPC code.
pub mod proto {
    tonic::include_proto!("com.apple.container.build.v1");
}

use proto::builder_client::BuilderClient;
use proto::{
    BuildTransfer, ClientStream, ImageTransfer, ServerStream, TransferDirection,
    client_stream, server_stream,
};

/// Container ID for the Apple Containers builder VM.
const BUILDER_CONTAINER_ID: &str = "buildkit";

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
    eprintln!("[build] ensuring builder VM...");
    ensure_builder(conn).await?;
    eprintln!("[build] builder ready");

    // Step 2: Connect via vsock (retry until the shim is listening).
    eprintln!("[build] dialing builder shim...");
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
        eprintln!("[build] got vsock fd: {fd} (attempt {attempt})");
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
            Ok(resp) => {
                eprintln!("[build] gRPC info() ok: {resp:?}");
                client = Some(c);
                break;
            }
            Err(e) => {
                eprintln!("[build] gRPC not ready (attempt {attempt}): {e}");
                if attempt == 29 {
                    return Err(AppleContainerError::XpcError(
                        format!("builder gRPC server not ready after 30s: {e}"),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    let _client = client.unwrap();

    // Step 3: Resolve context path.
    let abs_context = std::fs::canonicalize(context)
        .map_err(AppleContainerError::Io)?;

    // Step 4: Build via PerformBuild bidirectional stream.
    let build_id = uuid::Uuid::new_v4().to_string();
    let context_str = abs_context.to_string_lossy().to_string();

    eprintln!("[build] build_id: {build_id}");

    // Dial a fresh connection for PerformBuild. The info() call completes
    // the HTTP/2 handshake — every successful test had this warmup.
    let dockerfile_b64 = base64_encode(dockerfile.as_bytes());

    eprintln!("[build] dialing fresh vsock for PerformBuild...");
    let fd2 = dial_container(conn, BUILDER_CONTAINER_ID, 8088).await?;
    eprintln!("[build] got fresh vsock fd: {fd2}");
    let ch2 = dial_builder_channel(fd2).await?;
    let mut build_client = BuilderClient::new(ch2);

    build_client.info(proto::InfoRequest {}).await
        .map_err(|e| AppleContainerError::XpcError(format!("fresh info() failed: {e}")))?;
    eprintln!("[build] fresh connection info() OK");

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
    md.insert("outputs", "type=oci".parse().unwrap());
    if no_cache {
        md.insert("no-cache", "".parse().unwrap());
    }

    eprintln!("[build] calling PerformBuild...");
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        build_client.perform_build(request),
    ).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            eprintln!("[build] PerformBuild error: {e}");
            eprintln!("[build] PerformBuild error (debug): {e:?}");
            if let Some(source) = std::error::Error::source(&e) {
                eprintln!("[build] PerformBuild source: {source}");
                if let Some(inner) = std::error::Error::source(source) {
                    eprintln!("[build] PerformBuild inner: {inner}");
                }
            }
            return Err(AppleContainerError::XpcError(format!("PerformBuild failed: {e}")));
        }
        Err(_) => {
            eprintln!("[build] PerformBuild timed out");
            return Err(AppleContainerError::XpcError("PerformBuild timed out".to_string()));
        }
    };
    eprintln!("[build] PerformBuild stream opened");

    let mut server_stream = response.into_inner();

    // Process the bidirectional stream.
    process_build_stream(&mut server_stream, client_tx, &build_id, &abs_context, verbose).await
}

/// Process the PerformBuild bidirectional stream.
///
/// The server sends requests for files (BuildTransfer with fssync operations)
/// and build output (IO with stdout/stderr). We respond with file data.
async fn process_build_stream(
    server_stream: &mut tonic::Streaming<ServerStream>,
    client_tx: tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
    verbose: bool,
) -> Result<(), AppleContainerError> {
    use tokio_stream::StreamExt;

    while let Some(msg) = server_stream.next().await {
        let msg = msg.map_err(|e| {
            AppleContainerError::XpcError(format!("build stream error: {e}"))
        })?;

        match msg.packet_type {
            Some(server_stream::PacketType::Io(io)) => {
                handle_io(&io, verbose);
            }
            Some(server_stream::PacketType::BuildError(err)) => {
                return Err(AppleContainerError::XpcError(
                    format!("Build failed: {}", err.message)
                ));
            }
            Some(server_stream::PacketType::CommandComplete(_)) => {
                // A RUN command completed, continue.
            }
            Some(server_stream::PacketType::BuildTransfer(transfer)) => {
                handle_build_transfer(&transfer, &client_tx, build_id, context).await?;
            }
            Some(server_stream::PacketType::ImageTransfer(ref transfer)) => {
                let stage = transfer.metadata.get("stage").map(|s| s.as_str()).unwrap_or("");
                let method = transfer.metadata.get("method").map(|s| s.as_str()).unwrap_or("");
                if stage == "resolver" && method == "/resolve" {
                    handle_image_resolve(transfer, &client_tx, build_id).await?;
                }
            }
            None => {}
        }
    }

    Ok(())
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
    let stage = transfer.metadata.get("stage").map(|s| s.as_str()).unwrap_or("");
    let method = transfer.metadata.get("method").map(|s| s.as_str()).unwrap_or("");

    if stage != "fssync" {
        return Ok(());
    }

    match method {
        "walk" => {
            handle_walk(transfer, client_tx, build_id, context).await?;
        }
        "read" => {
            handle_read(transfer, client_tx, build_id, context).await?;
        }
        "info" => {
            handle_info(transfer, client_tx, build_id, context).await?;
        }
        _ => {
            eprintln!("[build] unknown fssync method: {method}");
        }
    }

    Ok(())
}

/// Handle a "walk" request — list files in the context directory.
/// Responds with file metadata as JSON, then a completion packet.
async fn handle_walk(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let include_patterns: Vec<String> = transfer.metadata.get("includePatterns")
        .map(|s| serde_json::from_str(s).unwrap_or_default())
        .unwrap_or_default();

    let mut entries = Vec::new();
    walk_dir(context, context, &include_patterns, &mut entries)?;

    let data = serde_json::to_vec(&entries)
        .map_err(AppleContainerError::Serialization)?;

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("stage".to_string(), "fssync".to_string());
    metadata.insert("method".to_string(), "walk".to_string());
    metadata.insert("mode".to_string(), "json".to_string());

    // Send file list.
    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::BuildTransfer(BuildTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Outof as i32,
            source: None,
            destination: None,
            data,
            complete: false,
            is_directory: false,
            metadata,
        })),
    };
    let _ = client_tx.send(response).await;

    // Send completion.
    let mut complete_meta = std::collections::HashMap::new();
    complete_meta.insert("stage".to_string(), "fssync".to_string());
    complete_meta.insert("method".to_string(), "walk".to_string());

    let complete = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::BuildTransfer(BuildTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Outof as i32,
            source: None,
            destination: None,
            data: Vec::new(),
            complete: true,
            is_directory: false,
            metadata: complete_meta,
        })),
    };
    let _ = client_tx.send(complete).await;

    Ok(())
}

/// Handle a "read" request — send file content.
async fn handle_read(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    eprintln!("[fssync] read request: source={:?}", transfer.source);
    let source = transfer.source.as_deref().unwrap_or("");
    let path = resolve_path(context, source);

    let data = if path.is_file() {
        let offset = transfer.metadata.get("offset")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let len = transfer.metadata.get("len")
            .and_then(|s| s.parse::<u64>().ok());

        let full = std::fs::read(&path).map_err(AppleContainerError::Io)?;

        let start = offset as usize;
        if start >= full.len() {
            Vec::new()
        } else if let Some(l) = len {
            let end = std::cmp::min(start + l as usize, full.len());
            full[start..end].to_vec()
        } else {
            full[start..].to_vec()
        }
    } else {
        Vec::new()
    };

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("stage".to_string(), "fssync".to_string());
    metadata.insert("method".to_string(), "read".to_string());

    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::BuildTransfer(BuildTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Outof as i32,
            source: transfer.source.clone(),
            destination: None,
            data,
            complete: true,
            is_directory: false,
            metadata,
        })),
    };
    let _ = client_tx.send(response).await;

    Ok(())
}

/// Handle an "info" request — send file metadata.
async fn handle_info(
    transfer: &BuildTransfer,
    client_tx: &tokio::sync::mpsc::Sender<ClientStream>,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let source = transfer.source.as_deref().unwrap_or("");
    let path = resolve_path(context, source);

    let data = if path.exists() {
        let meta = std::fs::metadata(&path).map_err(AppleContainerError::Io)?;
        let info = FileInfo::from_metadata(source, &meta);
        serde_json::to_vec(&info).map_err(AppleContainerError::Serialization)?
    } else {
        Vec::new()
    };

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("stage".to_string(), "fssync".to_string());
    metadata.insert("method".to_string(), "info".to_string());

    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::BuildTransfer(BuildTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Outof as i32,
            source: transfer.source.clone(),
            destination: None,
            data,
            complete: true,
            is_directory: false,
            metadata,
        })),
    };
    let _ = client_tx.send(response).await;

    Ok(())
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
) -> Result<(), AppleContainerError> {
    // The image reference is in metadata "ref" or the tag field.
    let reference = transfer.metadata.get("ref")
        .or_else(|| if transfer.tag.is_empty() { None } else { Some(&transfer.tag) })
        .ok_or_else(|| AppleContainerError::XpcError("image resolve: missing ref".into()))?;
    let platform_str = transfer.metadata.get("platform").map(|s| s.as_str()).unwrap_or("linux/arm64");

    eprintln!("[build] resolving image: {reference} for {platform_str}");

    let oci_ref: oci_client::Reference = reference.parse()
        .map_err(|e: oci_client::ParseError| AppleContainerError::XpcError(format!("invalid image ref: {e}")))?;

    let client = oci_client::Client::default();
    let auth = oci_client::secrets::RegistryAuth::Anonymous;

    client.auth(&oci_ref, &auth, oci_client::RegistryOperation::Pull).await
        .map_err(|e| AppleContainerError::XpcError(format!("registry auth failed: {e}")))?;

    let (manifest, digest) = client.pull_image_manifest(&oci_ref, &auth).await
        .map_err(|e| AppleContainerError::XpcError(format!("failed to pull manifest for {reference}: {e}")))?;

    // Pull the OCI image config blob.
    let mut config_data = Vec::new();
    client.pull_blob(&oci_ref, manifest.config.digest.as_str(), &mut config_data).await
        .map_err(|e| AppleContainerError::XpcError(format!("failed to pull config for {reference}: {e}")))?;

    eprintln!("[build] resolved {reference} -> {digest} (config {} bytes)", config_data.len());

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
            tag: digest,
            descriptor: None,
            data: config_data,
            complete: true,
            metadata,
        })),
    };
    let _ = client_tx.send(response).await;

    Ok(())
}

/// Resolve a source path relative to the context directory.
fn resolve_path(context: &Path, source: &str) -> std::path::PathBuf {
    if source.starts_with('/') {
        std::path::PathBuf::from(source)
    } else {
        context.join(source)
    }
}

/// Walk a directory and collect file info entries.
fn walk_dir(
    root: &Path,
    dir: &Path,
    _include_patterns: &[String],
    entries: &mut Vec<FileInfo>,
) -> Result<(), AppleContainerError> {
    let read_dir = std::fs::read_dir(dir).map_err(AppleContainerError::Io)?;

    let mut items: Vec<_> = read_dir
        .filter_map(|e| e.ok())
        .collect();
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        let path = entry.path();
        let name = path.strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Skip hidden files and common build artifacts.
        if name.starts_with('.') {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        entries.push(FileInfo::from_metadata(&name, &meta));

        if meta.is_dir() {
            walk_dir(root, &path, _include_patterns, entries)?;
        }
    }

    Ok(())
}

/// File metadata matching the Swift `FileInfo` struct.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FileInfo {
    name: String,
    size: u64,
    mode: u32,
    mod_time: i64,
    is_dir: bool,
    uid: u32,
    gid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_target: Option<String>,
}

impl FileInfo {
    fn from_metadata(name: &str, meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        use std::time::UNIX_EPOCH;

        let mod_time = meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        Self {
            name: name.to_string(),
            size: meta.len(),
            mode: meta.mode(),
            mod_time,
            is_dir: meta.is_dir(),
            uid: meta.uid(),
            gid: meta.gid(),
            link_target: None,
        }
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
        Self { writer, buf: [0; 3], len: 0 }
    }

    fn flush_buf(&mut self) -> std::io::Result<()> {
        if self.len == 0 { return Ok(()); }
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

    reply.dup_fd(XpcKey::FD).ok_or_else(|| {
        AppleContainerError::XpcError("containerDial reply missing fd".to_string())
    })
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
            fd, libc::SOL_SOCKET, libc::SO_SNDBUF,
            &send_buf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd, libc::SOL_SOCKET, libc::SO_RCVBUF,
            &recv_buf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true).map_err(AppleContainerError::Io)?;
    let tokio_stream = tokio::net::UnixStream::from_std(std_stream)
        .map_err(AppleContainerError::Io)?;

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
                    let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    eprintln!("[build] connector called (invocation #{n})");
                    let stream = stream_slot.lock().await.take().ok_or_else(|| {
                        eprintln!("[build] connector: stream already consumed!");
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
async fn pull_image(
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
async fn unpack_image(
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
async fn get_default_kernel(
    conn: &XpcConnection,
) -> Result<Vec<u8>, AppleContainerError> {
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
pub async fn ensure_builder(
    conn: &XpcConnection,
) -> Result<(), AppleContainerError> {
    // Step 1: Check if the builder container already exists.
    let snapshot = get_container(conn, BUILDER_CONTAINER_ID).await;

    match snapshot {
        Some(snap) if snap.status == RuntimeStatus::Running => {
            return Ok(());
        }
        Some(snap) if snap.status == RuntimeStatus::Stopped => {
            eprintln!("[build] builder exists but stopped, bootstrapping...");
            bootstrap_container(conn, BUILDER_CONTAINER_ID).await?;
            start_process(conn, BUILDER_CONTAINER_ID).await?;
            wait_for_running(conn, BUILDER_CONTAINER_ID).await?;
            return Ok(());
        }
        Some(_) => {
            // Unknown/Stopping — try bootstrap anyway.
            eprintln!("[build] builder in unexpected state, attempting bootstrap...");
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

    eprintln!("[build] fetching builder image...");
    let image_desc_bytes = pull_image(BUILDER_IMAGE, &platform_json).await?;
    let image_desc: serde_json::Value = serde_json::from_slice(&image_desc_bytes)?;
    eprintln!("[build] image descriptor: {}", image_desc);

    eprintln!("[build] unpacking builder image...");
    unpack_image(&image_desc_bytes, &platform_json).await?;

    eprintln!("[build] fetching default kernel...");
    let kernel_bytes = get_default_kernel(conn).await?;

    eprintln!("[build] creating builder VM...");
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let exports_dir = std::path::PathBuf::from(&home)
        .join("Library/Application Support/com.apple.container/builder");
    // Ensure the exports directory exists (the builder shim writes build
    // outputs here via virtiofs).
    let _ = std::fs::create_dir_all(&exports_dir);

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

    eprintln!("[build] bootstrapping builder VM...");
    bootstrap_container(conn, BUILDER_CONTAINER_ID).await?;

    eprintln!("[build] starting builder process...");
    start_process(conn, BUILDER_CONTAINER_ID).await?;

    wait_for_running(conn, BUILDER_CONTAINER_ID).await?;

    Ok(())
}

/// Get a container snapshot by ID, returning `None` if not found.
///
/// Uses `containerList` and filters by ID, since the list route returns
/// snapshot data under a well-known key (`containers`).
async fn get_container(
    conn: &XpcConnection,
    id: &str,
) -> Option<ContainerSnapshot> {
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
async fn bootstrap_container(
    conn: &XpcConnection,
    id: &str,
) -> Result<(), AppleContainerError> {
    let devnull = std::fs::File::open("/dev/null")
        .map_err(AppleContainerError::Io)?;
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
async fn start_process(
    conn: &XpcConnection,
    id: &str,
) -> Result<(), AppleContainerError> {
    let msg = XpcMessage::with_route(XpcRoute::ContainerStartProcess.as_str());
    msg.set_string(XpcKey::ID, id);
    msg.set_string(XpcKey::PROCESS_IDENTIFIER, id);

    let reply = conn.send_async(&msg).await?;
    reply.check_error()?;
    Ok(())
}

/// Poll until the container reaches Running status (up to ~30 seconds).
async fn wait_for_running(
    conn: &XpcConnection,
    id: &str,
) -> Result<(), AppleContainerError> {
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
