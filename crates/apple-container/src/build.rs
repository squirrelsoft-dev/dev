use std::collections::{HashMap, HashSet};
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

/// How long either direction of the build stream may go without progress.
///
/// Every stall in this protocol is silent on both sides — the shim's receivers
/// have no deadline — so without this a protocol mismatch presents as an
/// indefinite hang rather than an error. The window is wide enough that a long
/// silent `RUN` step cannot trip it.
///
/// It bounds sends as well as receives. This is a bidirectional gRPC stream, so
/// the two block each other: the shim only drains our packets while it is not
/// itself blocked handing us one, and this loop only drains its packets while it
/// is not blocked sending. Covering one direction alone leaves the deadlock the
/// deadline exists to break.
const BUILDER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How many replies may be queued for the gRPC writer at once.
///
/// This bounds the memory the outbound half can hold, and the bound is set by
/// the largest payload class on it: a content-store reply may reach
/// [`MAX_REPLY_PAYLOAD`], so the worst case when the writer falls behind is
/// this many of those — roughly 63 MiB at 16, against the ~8 MiB the 512 KiB
/// walk chunks would reach. Deep enough that the writer is never the thing
/// pacing a build, shallow enough that a slow one cannot queue a quarter of a
/// gigabyte.
const OUTBOUND_PACKETS: usize = 16;

/// How long the base-image resolver may wait to reach a registry.
///
/// `oci_client` leaves both of these unset, which is `None` all the way down to
/// `reqwest` — a black-holed connection through a captive portal or a hung
/// corporate proxy then never returns. The resolve is awaited from the build
/// loop, which is not reading the server stream while it runs, so
/// [`BUILDER_IDLE_TIMEOUT`] cannot fire behind it and the whole build hangs on
/// a TCP handshake nobody is going to answer.
const REGISTRY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a registry response may go without delivering a byte.
///
/// Inactivity rather than total duration (`reqwest::ClientBuilder::read_timeout`),
/// so a slow but progressing manifest or config download is not cut off.
const REGISTRY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the daemon has to pull a base image before the build gives up.
///
/// The same hang, one layer down: `imagePull` is a synchronous XPC send on a
/// blocking thread with no deadline of its own, awaited from the build loop
/// while the server stream goes unread.
///
/// Total duration, not inactivity — unlike [`REGISTRY_READ_TIMEOUT`], which
/// can be an inactivity deadline because `reqwest` is watching the bytes. One
/// XPC reply is the whole of what crosses this boundary: the daemon says
/// nothing until the pull is done, so there is no progress here to measure and
/// nothing that can tell a stalled pull from a slow one. The budget is
/// therefore sized for the slow one, and what it reports is what it measured.
const IMAGE_PULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

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
    let (client_tx, client_rx) = tokio::sync::mpsc::channel::<ClientStream>(OUTBOUND_PACKETS);
    let client_stream = tokio_stream::wrappers::ReceiverStream::new(client_rx);

    let mut request = tonic::Request::new(client_stream);
    let md = request.metadata_mut();
    md.insert("build-id", header_value("build-id", &build_id)?);
    md.insert("tag", header_value("tag", tag)?);
    md.insert(
        "progress",
        tonic::metadata::MetadataValue::from_static("plain"),
    );
    md.insert("target", tonic::metadata::MetadataValue::from_static(""));
    md.insert("context", header_value("context", &context_str)?);
    md.insert("dockerfile", header_value("dockerfile", &dockerfile_b64)?);
    // The Go server panics with "assignment to entry in nil map" if no outputs
    // header is sent — the default ExportEntry has a nil Attrs map. Sending
    // this forces the parseOutputCSV path which initialises the map properly.
    // `name` makes BuildKit annotate the exported layout with the tag, which
    // is what `imageLoad` registers the image under.
    md.insert(
        "outputs",
        header_value("outputs", &format!("type=oci,name={tag}"))?,
    );
    if no_cache {
        md.insert("no-cache", tonic::metadata::MetadataValue::from_static(""));
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
        BuilderSink::new(client_tx),
        &build_id,
        &abs_context,
        verbose,
    )
    .await?;

    // A finished stream only means BuildKit wrote its OCI layout; the image
    // does not exist to the daemon until it is loaded from that archive.
    register_built_image(&export.archive(), tag).await
}

/// Turn a value the environment supplied into a gRPC header value.
///
/// The build's context path and tag both become headers, and a header may not
/// carry a line break or a NUL. A directory name may — a newline in a folder
/// name is unusual but perfectly legal — so a workspace under such a path has
/// to be reported rather than panicked on.
fn header_value(
    name: &str,
    value: &str,
) -> Result<tonic::metadata::MetadataValue<tonic::metadata::Ascii>, AppleContainerError> {
    value.parse().map_err(|_| {
        AppleContainerError::XpcError(format!(
            "the build's {name} ({value:?}) cannot be sent to the builder: \
             a gRPC header may not carry a line break or a NUL"
        ))
    })
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
        let path = builder_exports_root()?.join(build_id);
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
///
/// The builder writes the image it just produced here and the daemon loads it
/// back, so it must be a directory only this user can write to — see
/// [`content::application_support_root`].
fn builder_exports_root() -> Result<std::path::PathBuf, AppleContainerError> {
    content::application_support_root()
        .map(|root| root.join("builder"))
        .ok_or_else(|| {
            AppleContainerError::XpcError(
                "HOME does not name an absolute directory, so the Apple Containers data \
                 directory cannot be located"
                    .to_string(),
            )
        })
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
    // the tag applied. Which one to tag is only unambiguous when the archive
    // held exactly one image: picking arbitrarily out of several would hand
    // `dev up` a container built from an image the build never asked for.
    let source = match loaded.as_slice() {
        [only] => only,
        [] => {
            return Err(AppleContainerError::XpcError(format!(
                "{} contained no image to register",
                archive.display()
            )));
        }
        many => {
            let references: Vec<&str> = many.iter().map(|i| i.reference.as_str()).collect();
            return Err(AppleContainerError::XpcError(format!(
                "{} contained several images and none is tagged {tag}: {}",
                archive.display(),
                references.join(", ")
            )));
        }
    };
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
        let rejected = rejected_members(&raw);
        if !rejected.is_empty() {
            return Err(AppleContainerError::XpcError(format!(
                "image archive contained files the daemon refused: {}",
                rejected.describe()
            )));
        }
    }

    let data = reply.get_data(XpcKey::IMAGE_DESCRIPTIONS).ok_or_else(|| {
        AppleContainerError::XpcError("imageLoad reply missing imageDescriptions".to_string())
    })?;
    Ok(serde_json::from_slice(&data)?)
}

/// What a `rejectedMembers` reply says the daemon refused.
///
/// Kept apart from the names because the two carry different weight: a member
/// this version cannot name is still a member it must not report as loaded.
#[derive(Debug, Default, PartialEq, Eq)]
struct RejectedMembers {
    named: Vec<String>,
    /// Entries in a shape this version cannot read a filename out of.
    unnamed: usize,
}

impl RejectedMembers {
    fn is_empty(&self) -> bool {
        self.named.is_empty() && self.unnamed == 0
    }

    fn describe(&self) -> String {
        let mut parts = self.named.clone();
        if self.unnamed > 0 {
            parts.push(format!("{} more this version cannot name", self.unnamed));
        }
        parts.join(", ")
    }
}

/// The archive members the daemon refused, read for what they plainly name.
///
/// This key's encoding is reverse-engineered from the daemon, so the rule is
/// the conservative one: report only what unambiguously names refused members,
/// and read anything else as nothing refused. Guessing the other way turns a
/// load that actually succeeded into a failed build.
///
/// The list is taken from a top-level array, or from the field of a wrapper
/// object that the reply key itself names — never from whichever key happens to
/// sort first, or `{"accepted":[...],"rejected":[]}` would report the accepted
/// files as the refused ones. A member that is a plain string is named; one in
/// a shape this version cannot read is counted instead, so a partial refusal is
/// still reported even when it cannot be spelled out.
fn rejected_members(raw: &[u8]) -> RejectedMembers {
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return RejectedMembers::default();
    };
    let members = match &payload {
        serde_json::Value::Array(members) => members.as_slice(),
        serde_json::Value::Object(fields) => {
            // The conversion is inside the lookup so a preferred key holding
            // something that is not a list — `{"rejectedMembers":null,
            // "rejected":["bad.tar"]}`, the natural encoding of an absent Swift
            // optional beside the real list — falls through to the next name
            // instead of shadowing it and reading as a clean load.
            match [XpcKey::REJECTED_MEMBERS, "rejected"]
                .iter()
                .find_map(|key| fields.get(*key).and_then(serde_json::Value::as_array))
            {
                Some(members) => members.as_slice(),
                None => return RejectedMembers::default(),
            }
        }
        _ => return RejectedMembers::default(),
    };

    let named: Vec<String> = members
        .iter()
        .filter_map(|member| member.as_str().map(str::to_string))
        .collect();
    RejectedMembers {
        unnamed: members.len() - named.len(),
        named,
    }
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
    builder: BuilderSink,
    _build_id: &str,
    context: &Path,
    verbose: bool,
) -> Result<(), AppleContainerError> {
    use tokio_stream::StreamExt;

    let mut session = BuildSession {
        context,
        resolved: None,
        pulled: Vec::new(),
        unanswered: HashSet::new(),
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
                send_io_ack(&builder, reply_id).await?;
            }
            Some(server_stream::PacketType::BuildError(err)) => {
                return Err(AppleContainerError::XpcError(format!(
                    "Build failed: {}",
                    err.message
                )));
            }
            Some(server_stream::PacketType::CommandComplete(ref _cmd)) => {}
            Some(server_stream::PacketType::BuildTransfer(transfer)) => {
                handle_build_transfer(&transfer, &builder, reply_id, &mut session).await?;
            }
            Some(server_stream::PacketType::ImageTransfer(ref transfer)) => {
                let stage = metadata_field(&transfer.metadata, "stage");
                let method = metadata_field(&transfer.metadata, "method");
                if !is_request(stage, method) {
                    note_unanswered_packet(&mut session.unanswered, "ImageTransfer", stage, method);
                    continue;
                }
                match (stage, method) {
                    ("resolver", "/resolve") => {
                        handle_image_resolve(transfer, &builder, reply_id, &mut session).await?;
                    }
                    ("content-store", _) => {
                        handle_content_store(transfer, method, &builder, reply_id, &mut session)
                            .await?;
                    }
                    // The shim's receivers have no deadline, so answering
                    // nothing leaves it waiting out the whole idle budget for
                    // a request we simply do not implement.
                    _ => {
                        let message = format!(
                            "this client does not implement the {stage:?} stage's {method:?} method"
                        );
                        send_image_error(&builder, reply_id, transfer, stage, method, &message)
                            .await?;
                    }
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
    /// Stage and method pairs already reported as unanswered, so a repeated
    /// ack or keepalive is noted once rather than once per packet.
    unanswered: HashSet<(String, String)>,
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

/// The outbound half of the build stream: where replies go, and how long one
/// may stall before the build gives up on it.
///
/// Pairing the deadline with the channel it governs keeps the two from
/// drifting, and every reply leaves through [`BuilderSink::send`] so the bound
/// is applied once rather than restated at each call site.
struct BuilderSink {
    packets: tokio::sync::mpsc::Sender<ClientStream>,
    /// How long a single send may make no progress. Only tests set this to
    /// anything but [`BUILDER_IDLE_TIMEOUT`].
    idle: std::time::Duration,
}

impl BuilderSink {
    fn new(packets: tokio::sync::mpsc::Sender<ClientStream>) -> Self {
        Self {
            packets,
            idle: BUILDER_IDLE_TIMEOUT,
        }
    }

    /// Hand one packet to the builder, or fail once the stream has stalled.
    ///
    /// A failure to enqueue is fatal in either form: the builder is waiting for
    /// this packet and has no deadline of its own, so a swallowed error — or an
    /// unbounded wait — hangs the build with nothing to report.
    async fn send(&self, message: ClientStream, what: &str) -> Result<(), AppleContainerError> {
        match tokio::time::timeout(self.idle, self.packets.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(AppleContainerError::XpcError(format!(
                "failed to send {what}: {e}"
            ))),
            Err(_) => Err(AppleContainerError::XpcError(format!(
                "builder stopped accepting packets for {}s while sending {what}; \
                 giving up on the build",
                self.idle.as_secs()
            ))),
        }
    }
}

/// Send an IO ack response.
///
/// The Go builder shim's StdioProxy.Write() blocks until the client sends a
/// `Run` command containing a base64-encoded `{"command_type":"terminal","code":"ack"}`
/// JSON payload.  Without this ack the entire build pipeline deadlocks.
async fn send_io_ack(builder: &BuilderSink, build_id: &str) -> Result<(), AppleContainerError> {
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

    builder.send(response, "the IO ack").await
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
    builder: &BuilderSink,
    build_id: &str,
    session: &mut BuildSession<'_>,
) -> Result<(), AppleContainerError> {
    let stage = metadata_field(&transfer.metadata, "stage");
    let method = metadata_field(&transfer.metadata, "method");
    let context = session.context;

    if !is_request(stage, method) {
        note_unanswered_packet(&mut session.unanswered, "BuildTransfer", stage, method);
        return Ok(());
    }

    // The builder shim sends capitalized method names (Walk, Read, Info).
    match (stage, method) {
        ("fssync", "walk" | "Walk") => handle_walk(transfer, builder, build_id, context).await,
        ("fssync", "read" | "Read") => handle_read(transfer, builder, build_id, context).await,
        ("fssync", "info" | "Info") => handle_info(transfer, builder, build_id, context).await,
        // The shim's receivers have no deadline, so a request answered with
        // silence leaves it blocked until the whole idle budget expires and the
        // user is told the builder went quiet, not that we ignored it.
        _ => {
            let message =
                format!("this client does not implement the {stage:?} stage's {method:?} method");
            send_transfer_error(builder, build_id, transfer, stage, method, &message).await
        }
    }
}

/// Read one metadata field the builder sent, or the empty string.
fn metadata_field<'a>(metadata: &'a HashMap<String, String>, key: &str) -> &'a str {
    metadata.get(key).map(String::as_str).unwrap_or("")
}

/// Whether a packet names a request this client can answer.
///
/// A reply — including an error reply — is addressed by the stage and method it
/// echoes, so a packet naming neither can be routed nowhere and is left alone.
///
/// Naming both is the whole test, and deliberately so. Neither of the other
/// fields separates a request from an echo of one of our own replies. The shim
/// builds its requests without setting `complete` at all — `packetReaderAt`
/// (`pkg/content/readerat.go`) and `packetReadAt` (`pkg/fssync/file.go`) fill
/// in id, direction, metadata and descriptor and leave it to default — so it is
/// false on every request and cannot distinguish one from a reply of ours that
/// is still mid-transfer. `direction` describes which way the data flows, so a
/// request for us to send data carries the same `OUTOF` our replies do.
/// Between answering a packet that did not need it and staying silent on one
/// that did, silence is the
/// worse failure: the shim's receivers have no deadline, so it costs the whole
/// 600s idle budget and the build dies with the wrong reason.
fn is_request(stage: &str, method: &str) -> bool {
    !stage.is_empty() && !method.is_empty()
}

/// Record a packet that was not a request, since nothing is sent in reply.
///
/// Once per stage and method per build: the shim may send an ack or keepalive
/// on every transfer, and this shares stderr with the build's own output, which
/// one line per packet would bury.
fn note_unanswered_packet(
    seen: &mut HashSet<(String, String)>,
    kind: &str,
    stage: &str,
    method: &str,
) {
    if !seen.insert((stage.to_string(), method.to_string())) {
        return;
    }
    eprintln!(
        "Warning: ignoring builder {kind} packets that are not requests \
         (stage {stage:?}, method {method:?})."
    );
}

/// Metadata every fssync reply carries.
fn fssync_metadata(method: &str) -> HashMap<String, String> {
    transfer_metadata("fssync", method)
}

/// Metadata a reply on any stage carries.
fn transfer_metadata(stage: &str, method: &str) -> HashMap<String, String> {
    HashMap::from([
        ("os".to_string(), "linux".to_string()),
        ("stage".to_string(), stage.to_string()),
        ("method".to_string(), method.to_string()),
    ])
}

/// Send one `BuildTransfer` reply on the request's id.
async fn send_build_transfer(
    builder: &BuilderSink,
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

    builder.send(response, "an fssync reply").await
}

/// Tell the builder a request failed instead of leaving it waiting.
///
/// Every shim receiver checks `metadata["error"]` first, so this turns what
/// would otherwise be an unbounded wait into a reported build failure.
async fn send_fssync_error(
    builder: &BuilderSink,
    build_id: &str,
    transfer: &BuildTransfer,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    send_transfer_error(builder, build_id, transfer, "fssync", method, message).await
}

/// Report a failure on whichever stage the request named.
async fn send_transfer_error(
    builder: &BuilderSink,
    build_id: &str,
    transfer: &BuildTransfer,
    stage: &str,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    let mut metadata = transfer_metadata(stage, method);
    metadata.insert("error".to_string(), message.to_string());
    send_build_transfer(
        builder,
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
/// the tar follows in chunks (`pkg/fileutils/tarxfer.go`). The archive is
/// produced twice — once to hash, once to send — rather than held in memory,
/// so a repository-sized context costs file reads instead of a multi-gigabyte
/// allocation.
async fn handle_walk(
    transfer: &BuildTransfer,
    builder: &BuilderSink,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let filter = fssync::ContextFilter::from_metadata(&transfer.metadata);
    let prepared = match prepare_walk(context, &filter, &transfer.metadata).await {
        Ok(prepared) => prepared,
        Err(e) => {
            // The failure to report is never more informative than the failure
            // it was reporting, so the original cause is what comes back.
            let _ = send_fssync_error(builder, build_id, transfer, "Walk", &e.to_string()).await;
            return Err(e);
        }
    };
    let (entries, checksum) = prepared;

    let mut hash_metadata = fssync_metadata("Walk");
    hash_metadata.insert("hash".to_string(), checksum.clone());
    send_build_transfer(
        builder,
        build_id,
        transfer,
        hash_metadata,
        Vec::new(),
        false,
        false,
    )
    .await?;

    stream_walk_archive(entries, &checksum, builder, build_id, transfer).await
}

/// Collect the context and hash the archive it will produce.
///
/// Both walk the tree and read every selected file, so they run on the
/// blocking pool rather than on a runtime worker.
async fn prepare_walk(
    context: &Path,
    filter: &fssync::ContextFilter,
    metadata: &HashMap<String, String>,
) -> Result<(Vec<fssync::ContextEntry>, String), AppleContainerError> {
    fssync::require_tar_walk_mode(metadata)?;
    let context = context.to_path_buf();
    let filter = filter.clone();
    blocking(move || {
        let entries = fssync::collect_context(&context, &filter)?;
        let checksum = fssync::context_tar_checksum(&entries)?;
        Ok((entries, checksum))
    })
    .await
}

/// Send the archive as `BuildTransfer` packets, one chunk at a time.
///
/// The last packet is the one that carries `complete`, so each chunk is held
/// back until the next arrives — and a failure part-way through is reported as
/// an fssync error rather than as a truncated archive the shim would accept.
///
/// The bytes handed over are digested as they go and checked against the
/// `announced` checksum before the transfer is completed. The two passes read
/// the same files at different moments, so a file rewritten in between would
/// otherwise register content under a hash that does not describe it — and the
/// shim caches its unpacked context by that hash, so a later stable build would
/// silently reuse the wrong tree.
///
/// The producing direction is bounded too. The writer's `blocking_send` needs
/// no deadline of its own — it can only block while this loop is draining, and
/// every exit from the loop closes the receiver, which fails the pending send
/// rather than leaving it parked. But the filesystem underneath it can stall (a
/// hung NFS or virtiofs mount, a device that never answers), and nothing else
/// in this protocol would ever notice, so each wait for the next chunk carries
/// the same idle budget every send does. A thread already inside such a call
/// cannot be reclaimed — `spawn_blocking` work is not cancellable — so the
/// deadline frees the build, not the thread.
async fn stream_walk_archive(
    entries: Vec<fssync::ContextEntry>,
    announced: &str,
    builder: &BuilderSink,
    build_id: &str,
    transfer: &BuildTransfer,
) -> Result<(), AppleContainerError> {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let writer = tokio::task::spawn_blocking(move || {
        let mut digest = fssync::ArchiveDigest::default();
        {
            let mut emit = |chunk: Vec<u8>| {
                digest.update(&chunk);
                chunk_tx.blocking_send(chunk).map_err(|_| {
                    AppleContainerError::XpcError(
                        "build context transfer was abandoned".to_string(),
                    )
                })
            };
            fssync::stream_context_tar(&entries, &mut emit)?;
        }
        Ok(digest.finish())
    });

    match send_archive_chunks(
        &mut chunk_rx,
        writer,
        announced,
        builder,
        build_id,
        transfer,
    )
    .await
    {
        Ok(pending) => {
            // `readTarHeader` blocks until at least one data packet arrives, so
            // an empty context still has to send its end-of-archive marker.
            send_build_transfer(
                builder,
                build_id,
                transfer,
                fssync_metadata("Walk"),
                pending.unwrap_or_default(),
                true,
                false,
            )
            .await
        }
        Err(WalkFailure::Reportable(e)) => {
            // The failure to report is never more informative than the failure
            // it was reporting, so the original cause is what comes back.
            let _ = send_fssync_error(builder, build_id, transfer, "Walk", &e.to_string()).await;
            Err(e)
        }
        Err(WalkFailure::SinkFailed(e)) => Err(e),
    }
}

/// Why a walk stopped, and whether the builder can still be told.
enum WalkFailure {
    /// The context could not be produced. The stream out is still usable, so
    /// the builder gets an error packet rather than an unbounded wait.
    Reportable(AppleContainerError),
    /// The stream out is what failed. Sending an error packet on it would only
    /// spend a second idle budget waiting on the same wedged channel, and would
    /// bury the failure that actually happened.
    SinkFailed(AppleContainerError),
}

/// Drain the archive into `BuildTransfer` packets, returning the last chunk.
///
/// The final packet is the one carrying `complete`, so the last chunk is held
/// back for the caller to send once the archive has been checked against the
/// checksum the shim was promised.
async fn send_archive_chunks(
    chunks: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    writer: tokio::task::JoinHandle<Result<String, AppleContainerError>>,
    announced: &str,
    builder: &BuilderSink,
    build_id: &str,
    transfer: &BuildTransfer,
) -> Result<Option<Vec<u8>>, WalkFailure> {
    // Closing the receiver fails the writer's next `blocking_send` instead of
    // leaving it parked, which is all that can be reclaimed: a thread already
    // inside a filesystem call that never returns stays there either way.
    let give_up =
        |chunks: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
         writer: &tokio::task::JoinHandle<Result<String, AppleContainerError>>| {
            chunks.close();
            writer.abort();
            WalkFailure::Reportable(AppleContainerError::XpcError(format!(
                "the build context produced nothing for {}s; giving up on the build",
                builder.idle.as_secs()
            )))
        };

    let mut pending: Option<Vec<u8>> = None;
    loop {
        let next = match tokio::time::timeout(builder.idle, chunks.recv()).await {
            Ok(next) => next,
            Err(_) => return Err(give_up(chunks, &writer)),
        };
        let Some(chunk) = next else { break };
        if let Some(previous) = pending.replace(chunk) {
            send_build_transfer(
                builder,
                build_id,
                transfer,
                fssync_metadata("Walk"),
                previous,
                false,
                false,
            )
            .await
            .map_err(WalkFailure::SinkFailed)?;
        }
    }

    // The sender is dropped when the writer finishes, so the loop above has
    // already ended by the time this resolves.
    let streamed = match tokio::time::timeout(builder.idle, join_blocking(writer)).await {
        Ok(streamed) => streamed.map_err(WalkFailure::Reportable)?,
        Err(_) => {
            chunks.close();
            return Err(WalkFailure::Reportable(AppleContainerError::XpcError(
                format!(
                    "the build context did not finish within {}s; giving up on the build",
                    builder.idle.as_secs()
                ),
            )));
        }
    };
    if streamed != announced {
        return Err(WalkFailure::Reportable(AppleContainerError::XpcError(
            format!(
                "build context changed while it was being sent: \
                 announced {announced}, sent {streamed}"
            ),
        )));
    }
    Ok(pending)
}

/// Run blocking filesystem work off the runtime's worker threads.
async fn blocking<T, F>(work: F) -> Result<T, AppleContainerError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppleContainerError> + Send + 'static,
{
    join_blocking(tokio::task::spawn_blocking(work)).await
}

/// Unwrap the two error layers a blocking task can fail with.
async fn join_blocking<T>(
    handle: tokio::task::JoinHandle<Result<T, AppleContainerError>>,
) -> Result<T, AppleContainerError> {
    handle.await.map_err(|e| {
        AppleContainerError::XpcError(format!("build context task did not finish: {e}"))
    })?
}

/// Answer an fssync `Read` with a slice of a context file.
///
/// The shim sends the caller's buffer size as `length` (`pkg/fssync/file.go`)
/// and reads an empty reply as EOF. An absent `length` therefore falls back to
/// a packet's worth of the file, not to zero: a shim that renames or drops the
/// key would otherwise silently build an image whose files are all empty.
async fn handle_read(
    transfer: &BuildTransfer,
    builder: &BuilderSink,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    let source = transfer.source.as_deref().unwrap_or("");
    let window = match requested_read_window(&transfer.metadata) {
        Ok(window) => window,
        Err(reason) => {
            let message = format!("cannot read {source}: {reason}");
            return send_fssync_error(builder, build_id, transfer, "Read", &message).await;
        }
    };
    let offset = window.offset;
    let requested = fssync_read_length(window.length);

    let (context, named) = (context.to_path_buf(), source.to_string());
    let read = blocking(move || read_context_range(&context, &named, offset, requested)).await;
    let data = match read {
        Ok(data) => data,
        Err(e) => {
            let message = format!("cannot read {source}: {e}");
            return send_fssync_error(builder, build_id, transfer, "Read", &message).await;
        }
    };

    let mut metadata = fssync_metadata("Read");
    metadata.insert("offset".to_string(), offset.to_string());
    metadata.insert("length".to_string(), data.len().to_string());
    send_build_transfer(builder, build_id, transfer, metadata, data, true, false).await
}

/// Read the window an fssync `Read` asked for out of the build context.
///
/// Confining the *name* is not enough to open it safely. `resolve_readable_path`
/// keeps the read inside the context, but says nothing about what the path has
/// become: a named pipe there — and a workspace can hold one, the fixtures are
/// literally called `dev-server.fifo` — blocks the opening thread until a writer
/// appears, with no deadline anywhere above this call. The walk already opens
/// context files through [`fssync::open_regular`] for that reason, so this takes
/// the same door rather than a second one with different rules.
fn read_context_range(
    context: &Path,
    source: &str,
    offset: u64,
    requested: u64,
) -> Result<Vec<u8>, AppleContainerError> {
    let path = resolve_readable_path(context, source)?;
    let (mut file, metadata) = fssync::open_regular(&path, source)?;
    let available = metadata.len().saturating_sub(offset);
    content::read_range_of(&mut file, offset, readable_span(requested, available)?)
}

/// Answer an fssync `Info` with a context path's metadata.
///
/// The shim reads size, mode, timestamp and ownership out of the reply's
/// *metadata* map (`pkg/fileutils/file_info.go`); anything left out silently
/// becomes a zero, so a JSON body in `data` reads as an empty file.
async fn handle_info(
    transfer: &BuildTransfer,
    builder: &BuilderSink,
    build_id: &str,
    context: &Path,
) -> Result<(), AppleContainerError> {
    use std::os::unix::fs::MetadataExt;

    let source = transfer.source.as_deref().unwrap_or("");
    let path = match resolve_stat_path(context, source) {
        Ok(path) => path,
        Err(e) => {
            let message = format!("cannot stat {source}: {e}");
            return send_fssync_error(builder, build_id, transfer, "Info", &message).await;
        }
    };

    // `symlink_metadata` describes the link itself; BuildKit resolves links
    // on its own side and expects to be told the target.
    let file = match std::fs::symlink_metadata(&path) {
        Ok(file) => file,
        Err(e) => {
            // A missing path is routine — BuildKit probes for `.dockerignore`
            // on every build — so report it and let the builder decide.
            let message = format!("cannot stat {source}: {e}");
            return send_fssync_error(builder, build_id, transfer, "Info", &message).await;
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
    if file.is_symlink()
        && let Ok(target) = std::fs::read_link(&path)
    {
        metadata.insert("target".to_string(), target.to_string_lossy().into_owned());
    }

    send_build_transfer(
        builder,
        build_id,
        transfer,
        metadata,
        Vec::new(),
        true,
        file.is_dir(),
    )
    .await
}

/// The most payload one reply may carry.
///
/// gRPC's default message limit is 4 MiB and a reply carries its metadata map
/// as well, so the payload stops a little under it.
///
/// This is a ceiling, not a working size. The shim reads a layer through a
/// prefetcher whose `ChunkSize` is 1 MiB (`pkg/content/readerat.go`), so a
/// content-store request asks for well under this and it never binds in
/// practice; a request that would exceed it is refused rather than answered
/// short. See [`send_blob_range`].
const MAX_REPLY_PAYLOAD: u64 = 4 * 1024 * 1024 - 64 * 1024;

/// What an fssync `Read` that named no length is taken to have asked for.
///
/// `File.ReadAt` (`pkg/fssync/file.go`) sends its caller's buffer size as
/// `length` and reads an empty payload as EOF, so an absent `length` has to
/// mean "some of the file" rather than "none of it" — defaulting to zero would
/// report every context file as empty instead of failing. One reply's worth is
/// the guess, since that is the most a reply can carry either way.
///
/// The content store answers the same absence differently: its reads are whole
/// or refused, so [`send_blob_range`] reads it as the rest of the blob.
fn fssync_read_length(named: Option<u64>) -> u64 {
    named.unwrap_or(MAX_REPLY_PAYLOAD)
}

/// How much of a requested range one fssync `Read` reply carries.
///
/// Clamped by the end of the file, which is EOF and what `File.ReadAt`
/// (`pkg/fssync/file.go`) is asking to be told: a reply shorter than the length
/// it asked for sets `io.EOF` on the read.
///
/// That is exactly why a range the file *can* serve but one reply cannot is
/// refused instead. Cutting it to what fits would be read as the end of the
/// file, and the shim would stop there — the built image would carry a silently
/// truncated copy with nothing said on either side. The content store refuses
/// the same case for the same reason; see [`send_blob_range`]. Nothing in the
/// verified protocol asks for this much (`fssync_read_length` defaults to one
/// reply's worth, and real callers' buffers are far below it), so the refusal
/// costs nothing a truncation would not cost far more.
fn readable_span(requested: u64, available: u64) -> Result<usize, AppleContainerError> {
    let wanted = requested.min(available);
    if wanted > MAX_REPLY_PAYLOAD {
        return Err(AppleContainerError::XpcError(format!(
            "{wanted} bytes are needed to answer this read, more than the {MAX_REPLY_PAYLOAD} \
             one reply can carry; answering short would be taken for the end of the file"
        )));
    }
    Ok(wanted as usize)
}

/// Why a builder-supplied path was refused.
const UNCONFINED: &str = "it resolves outside the build context";

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

/// Picks the image-index entry to pull, in the shape `oci_client` expects.
type ImageIndexResolver =
    Box<dyn Fn(&[oci_client::manifest::ImageIndexEntry]) -> Option<String> + Send + Sync>;

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
fn platform_resolver(platform: &str) -> ImageIndexResolver {
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
    builder: &BuilderSink,
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
        connect_timeout: Some(REGISTRY_CONNECT_TIMEOUT),
        read_timeout: Some(REGISTRY_READ_TIMEOUT),
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
    builder.send(response, "the resolver reply").await
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
    builder: &BuilderSink,
    build_id: &str,
    session: &mut BuildSession<'_>,
) -> Result<(), AppleContainerError> {
    if !matches!(method, CONTENT_INFO_METHOD | CONTENT_READER_AT_METHOD) {
        // Answering nothing would leave the shim's deadline-free receiver
        // blocked until the whole idle budget expired.
        let message = format!("this client does not implement the content-store method {method:?}");
        return send_content_error(builder, build_id, transfer, method, &message).await;
    }

    let Some(digest) = content_digest(transfer) else {
        return send_content_error(
            builder,
            build_id,
            transfer,
            method,
            "content-store request named no digest",
        )
        .await;
    };

    // Read before anything is fetched on the request's behalf: a request whose
    // own numbers cannot be read should not send us pulling an image for it.
    let window = match requested_read_window(&transfer.metadata) {
        Ok(window) => window,
        Err(message) => {
            return send_content_error(builder, build_id, transfer, method, &message).await;
        }
    };

    let size = match ensure_blob(&digest, session).await {
        Ok(size) => size,
        Err(e) => {
            return send_content_error(builder, build_id, transfer, method, &e.to_string()).await;
        }
    };

    let offset = window.offset;
    if method == CONTENT_READER_AT_METHOD {
        let Some(path) = content::blob_path(&digest) else {
            let message = format!("content store: malformed digest {digest:?}");
            return send_content_error(builder, build_id, transfer, method, &message).await;
        };
        let range = BlobRange {
            digest: &digest,
            path: &path,
            method,
            size,
            offset,
            requested: window.length,
        };
        return send_blob_range(builder, build_id, transfer, &range).await;
    }

    // `Info` answers out of the metadata map alone.
    let metadata = blob_reply_metadata(method, &digest, size, offset, 0);
    send_image_transfer(
        builder,
        build_id,
        transfer,
        &digest,
        metadata,
        Vec::new(),
        true,
    )
    .await
}

/// What a request said about one of its numeric fields.
///
/// A value this client cannot read is deliberately not folded into "absent":
/// the two call for opposite answers, and guessing on the caller's behalf would
/// serve a windowed read something it never asked for — for `offset`, bytes
/// from somewhere else in the blob entirely.
#[derive(Debug, PartialEq, Eq)]
enum RequestedNumber<'a> {
    Named(u64),
    Absent,
    Unreadable(&'a str),
}

fn requested_number<'a>(metadata: &'a HashMap<String, String>, key: &str) -> RequestedNumber<'a> {
    match metadata.get(key) {
        None => RequestedNumber::Absent,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(value) => RequestedNumber::Named(value),
            Err(_) => RequestedNumber::Unreadable(raw),
        },
    }
}

/// Where a read wants to start, and how much of it was asked for.
#[derive(Debug, PartialEq, Eq)]
struct ReadWindow {
    /// Both protocols probe at offset 0 to learn a size, so an absent offset
    /// means the start.
    offset: u64,
    /// The caller's buffer size, or `None` when the request named no length.
    /// What that absence means is the caller's to decide: an fssync `Read`
    /// loops and takes a packet's worth, a content-store read takes the rest
    /// of the blob.
    length: Option<u64>,
}

/// Read the window a request named, or say why it cannot be read.
///
/// Both read protocols carry these two numbers under the same keys and both
/// fail the same way, so both are read through this. A number this client
/// cannot read is refused rather than defaulted, because neither receiver
/// checks what it gets back: `readerAt.ReadAt` (`pkg/content/readerat.go`) and
/// `File.ReadAt` (`pkg/fssync/file.go`) both ignore the offset a reply reports
/// and copy the payload straight into the buffer they asked for, so bytes
/// served from a guessed position are stored as though they came from the
/// requested one.
fn requested_read_window(metadata: &HashMap<String, String>) -> Result<ReadWindow, String> {
    let offset = match requested_number(metadata, "offset") {
        RequestedNumber::Named(offset) => offset,
        RequestedNumber::Absent => 0,
        RequestedNumber::Unreadable(raw) => {
            return Err(format!("this read named an unreadable offset {raw:?}"));
        }
    };
    let length = match requested_number(metadata, "length") {
        RequestedNumber::Named(length) => Some(length),
        RequestedNumber::Absent => None,
        RequestedNumber::Unreadable(raw) => {
            return Err(format!("this read named an unreadable length {raw:?}"));
        }
    };
    Ok(ReadWindow { offset, length })
}

/// The slice of a stored blob one `ReaderAt` asked for.
struct BlobRange<'a> {
    digest: &'a str,
    path: &'a Path,
    method: &'a str,
    /// The blob's whole size, which every reply reports.
    size: u64,
    offset: u64,
    /// The caller's buffer size, or `None` when the request named no length.
    requested: Option<u64>,
}

/// Answer a `ReaderAt` with one packet carrying as much of the range as it can.
///
/// One request, one reply — established from the shim's own source rather than
/// inferred. `readerAt.ReadAt` (`pkg/content/readerat.go`) issues one
/// `proxy.request` and uses the single `ImageTransfer` it returns:
/// `ContentStoreProxy.request` (`pkg/content/content.go`) forwards to
/// `Request` (`pkg/stream/stage.go`), which does exactly one `demux.Recv()`.
/// Nothing on this path reassembles packets, and nothing reads `complete`. A
/// second packet would not extend the answer, and it would not reach the next
/// read either: `request` mints a fresh uuid per call and uses it as both the
/// stream's build id and the demux key, then closes that demux when it returns,
/// so `UnimplementedBaseStage.run` (`pkg/stream/stage.go`) discards the extra
/// packet as one with no matching — or an already closed — handler. It would
/// simply be thrown away, and the bytes it carried lost.
///
/// So the answer is one packet, and a short answer is what the protocol
/// expects: `ReadAt` returns `(n, nil)` for `n < len(p)` and its caller reads
/// on from the new offset. `readerAt` is wrapped in a prefetcher configured
/// with `ChunkSize: 1 << 20`, so a request never asks for more than 1 MiB in
/// practice and [`MAX_REPLY_PAYLOAD`] is headroom rather than a limit that
/// bites. A request that would exceed it is refused rather than cut short,
/// because the prefetcher's `fetchChunk` (`pkg/prefetch/prefetch.go`) caches a
/// short read as a chunk of that length with no refill — a quietly truncated
/// reply would be compacted against the next chunk and corrupt the layer.
///
/// The contrast is the fssync `Walk`, which *is* multi-packet:
/// `pkg/fileutils/tarxfer.go` accumulates `bt.Data` in a loop and stops on
/// `bt.Complete`. Two receivers, two shapes — which is why this one cannot be
/// written by analogy to that one.
async fn send_blob_range(
    builder: &BuilderSink,
    build_id: &str,
    transfer: &ImageTransfer,
    range: &BlobRange<'_>,
) -> Result<(), AppleContainerError> {
    let available = range.size.saturating_sub(range.offset);
    // `ReaderAt::init` probes with length 0 purely to learn the size, and a
    // read that starts past the end is EOF; both are one empty reply.
    //
    // A named length is the caller's buffer; naming none asks for what is left
    // of the blob. Either way this reply is the whole answer, so a span that
    // does not fit in one is refused rather than cut down to what does — the
    // caller cannot tell a clamped reply from a complete one.
    let asked_for = match range.requested {
        Some(named) => named.min(available),
        None => available,
    };
    if asked_for > MAX_REPLY_PAYLOAD {
        let message = format!(
            "blob {} needs {asked_for} bytes from offset {} to answer this read, more than \
             the {MAX_REPLY_PAYLOAD} one reply can carry; answering short would be taken \
             for the whole range",
            range.digest, range.offset
        );
        return send_content_error(builder, build_id, transfer, range.method, &message).await;
    }
    let wanted = asked_for as usize;

    let (blob, at) = (range.path.to_path_buf(), range.offset);
    let read = blocking(move || content::read_range(&blob, at, wanted)).await;
    let data = match read {
        Ok(data) => data,
        Err(e) => {
            let message = format!("cannot read blob {}: {e}", range.digest);
            return send_content_error(builder, build_id, transfer, range.method, &message).await;
        }
    };

    // `read_range` clamps to the end of the file, so a read that came back
    // short of what the store's own size says is there means the blob was
    // replaced or collected under us. Answering with it would hand the shim a
    // layer whose digest cannot match, and `ReadAt` checks `metadata["error"]`
    // before it looks at the payload, so the disagreement is reported instead.
    if data.len() < wanted {
        let short_by = wanted - data.len();
        let message = format!(
            "blob {} came back {short_by} bytes short of the {wanted} this read asked for at \
             offset {}, against the {} the content store reported; it changed while it was \
             being read",
            range.digest, range.offset, range.size
        );
        return send_content_error(builder, build_id, transfer, range.method, &message).await;
    }

    let length = data.len() as u64;
    let metadata =
        blob_reply_metadata(range.method, range.digest, range.size, range.offset, length);
    send_image_transfer(
        builder,
        build_id,
        transfer,
        range.digest,
        metadata,
        data,
        true,
    )
    .await
}

/// Metadata one content-store reply carries.
fn blob_reply_metadata(
    method: &str,
    digest: &str,
    size: u64,
    offset: u64,
    length: u64,
) -> HashMap<String, String> {
    let mut metadata = content_store_metadata(method);
    metadata.insert("size".to_string(), size.to_string());
    metadata.insert("offset".to_string(), offset.to_string());
    metadata.insert("length".to_string(), length.to_string());
    // Both timestamps are parsed as RFC 3339; the store keeps no creation
    // time of its own, so report the blob's mtime for both.
    let stored_at = blob_timestamp(digest);
    metadata.insert("created_at".to_string(), stored_at.clone());
    metadata.insert("updated_at".to_string(), stored_at);
    metadata
}

/// Metadata every content-store reply carries.
fn content_store_metadata(method: &str) -> HashMap<String, String> {
    transfer_metadata("content-store", method)
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
///
/// Why the pull failed is what the user needs: a registry auth, network or
/// rate-limit failure reported as "not in the local content store" points at
/// the wrong subsystem entirely.
async fn ensure_blob(
    digest: &str,
    session: &mut BuildSession<'_>,
) -> Result<u64, AppleContainerError> {
    if let Some(size) = content::blob_size(digest) {
        return Ok(size);
    }

    let Some(resolved) = session.resolved.as_ref() else {
        return Err(AppleContainerError::XpcError(format!(
            "blob {digest} is not in the local content store and the build has resolved no \
             base image to pull it from"
        )));
    };
    if session.pulled.iter().any(|r| r == &resolved.reference) {
        return Err(AppleContainerError::XpcError(format!(
            "blob {digest} is still not in the local content store after pulling {}",
            resolved.reference
        )));
    }
    let (reference, platform) = (resolved.reference.clone(), resolved.platform.clone());
    session.pulled.push(reference.clone());

    let (os, architecture) = split_platform(&platform);
    let platform_json = serde_json::to_vec(&serde_json::json!({
        "os": os,
        "architecture": architecture,
    }))?;
    tokio::time::timeout(IMAGE_PULL_TIMEOUT, pull_image(&reference, &platform_json))
        .await
        .map_err(|_| {
            AppleContainerError::XpcError(format!(
                "pulling {reference} to supply blob {digest} did not finish within {}s; \
                 giving up on the build. The daemon reports nothing until a pull is done, so \
                 this may be a stalled pull or simply an image too large for the link",
                IMAGE_PULL_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| {
            AppleContainerError::XpcError(format!(
                "could not pull {reference} to supply blob {digest}: {e}"
            ))
        })?;

    content::blob_size(digest).ok_or_else(|| {
        AppleContainerError::XpcError(format!(
            "{reference} was pulled but does not contain blob {digest}"
        ))
    })
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
    builder: &BuilderSink,
    build_id: &str,
    transfer: &ImageTransfer,
    digest: &str,
    metadata: HashMap<String, String>,
    data: Vec<u8>,
    complete: bool,
) -> Result<(), AppleContainerError> {
    let response = ClientStream {
        build_id: build_id.to_string(),
        packet_type: Some(client_stream::PacketType::ImageTransfer(ImageTransfer {
            id: transfer.id.clone(),
            direction: TransferDirection::Into as i32,
            tag: digest.to_string(),
            descriptor: transfer.descriptor.clone(),
            data,
            complete,
            metadata,
        })),
    };

    builder.send(response, "a content-store reply").await
}

/// Report a content-store failure rather than leaving the builder waiting.
async fn send_content_error(
    builder: &BuilderSink,
    build_id: &str,
    transfer: &ImageTransfer,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    send_image_error(
        builder,
        build_id,
        transfer,
        "content-store",
        method,
        message,
    )
    .await
}

/// Report a failure on whichever image stage the request named.
async fn send_image_error(
    builder: &BuilderSink,
    build_id: &str,
    transfer: &ImageTransfer,
    stage: &str,
    method: &str,
    message: &str,
) -> Result<(), AppleContainerError> {
    let mut metadata = transfer_metadata(stage, method);
    metadata.insert("error".to_string(), message.to_string());
    send_image_transfer(
        builder,
        build_id,
        transfer,
        &transfer.tag.clone(),
        metadata,
        Vec::new(),
        true,
    )
    .await
}

/// Resolve a builder-supplied source path inside the build context.
///
/// The builder names the paths it wants and we hand back their contents, so an
/// unconfined `source` is a read of any file this user can reach: an fssync
/// `Read` for `/Users/me/.ssh/id_rsa` or `../../../.ssh/id_rsa` would be
/// streamed straight into the VM. `content::blob_path` defends the content
/// store against exactly this, and the fssync side is confined the same way —
/// an absolute path is only accepted when it names something inside the
/// context, and `..` is refused outright rather than normalised.
fn resolve_path(context: &Path, source: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;

    let source = Path::new(source);
    let relative = match source.is_absolute() {
        true => source.strip_prefix(context).ok()?,
        false => source,
    };

    let mut resolved = context.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}

/// Resolve a path the builder wants the *contents* of.
///
/// Reading opens the file, and opening follows symlinks, so the name being
/// confined is not enough: a link inside the workspace — `deploy/key ->
/// ~/.ssh/id_rsa`, which a dependency or an earlier tool may well have left
/// there — resolves to a path under the context but reads a file outside it.
/// The resolved path is therefore canonicalized and re-checked against the
/// canonical context, so only a file that really lives inside it is read.
///
/// `Info` cannot use this: it must describe the *final* component as the link
/// it is, exactly as BuildKit's own fsutil sends the link rather than its
/// target, and canonicalizing would resolve it away. It is confined by
/// [`resolve_stat_path`] instead.
fn resolve_readable_path(
    context: &Path,
    source: &str,
) -> Result<std::path::PathBuf, AppleContainerError> {
    let unconfined = || AppleContainerError::XpcError(UNCONFINED.to_string());
    let path = resolve_path(context, source).ok_or_else(unconfined)?;
    let resolved = std::fs::canonicalize(&path).map_err(AppleContainerError::Io)?;
    let root = std::fs::canonicalize(context).map_err(AppleContainerError::Io)?;
    if !resolved.starts_with(&root) {
        return Err(unconfined());
    }
    Ok(resolved)
}

/// Resolve a path the builder wants the *metadata* of.
///
/// `resolve_path` refuses `..` but never normalises, so an intermediate symlink
/// is still followed when the path is used: a context holding `deploy/etc ->
/// /etc` — a link a dotfiles tool or a dependency can leave behind — answers an
/// `Info` for `deploy/etc/passwd` with a host file's size, mode and ownership.
/// The directory chain is therefore canonicalized and re-checked against the
/// canonical context, the same confinement [`resolve_readable_path`] applies.
///
/// Only the chain, though: the last component is rejoined unresolved so
/// `symlink_metadata` still describes a link as a link, which is what the shim
/// is asking for.
fn resolve_stat_path(
    context: &Path,
    source: &str,
) -> Result<std::path::PathBuf, AppleContainerError> {
    let unconfined = || AppleContainerError::XpcError(UNCONFINED.to_string());
    let path = resolve_path(context, source).ok_or_else(unconfined)?;
    let root = std::fs::canonicalize(context).map_err(AppleContainerError::Io)?;
    // The context itself names no component to hold back.
    if path == context {
        return Ok(root);
    }

    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(unconfined());
    };
    let parent = std::fs::canonicalize(parent).map_err(AppleContainerError::Io)?;
    if !parent.starts_with(&root) {
        return Err(unconfined());
    }
    Ok(parent.join(name))
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
    let exports_dir = builder_exports_root()?;
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

    /// A sink whose sends are bounded the way production's are.
    fn sink(packets: &tokio::sync::mpsc::Sender<ClientStream>) -> BuilderSink {
        BuilderSink::new(packets.clone())
    }

    /// A sink that gives up on a stalled send almost immediately.
    ///
    /// The deadline tests need to reach the give-up path, and the real budget
    /// is ten minutes. Shortening the budget rather than accelerating the clock
    /// keeps them on a real timer: `stream_walk_archive` drives its archive on
    /// the blocking pool, and a paused clock deliberately stops advancing while
    /// blocking work is outstanding, so a test written against one would be
    /// reasoning about the runtime's bookkeeping instead of the deadline.
    fn impatient_sink(packets: &tokio::sync::mpsc::Sender<ClientStream>) -> BuilderSink {
        BuilderSink {
            packets: packets.clone(),
            idle: std::time::Duration::from_millis(50),
        }
    }

    /// Await something that must bound itself, failing rather than hanging if
    /// it does not.
    ///
    /// The backstop is far above [`impatient_sink`]'s budget, so it is only
    /// reached when the work under test registered no deadline at all — a
    /// regression that drops the bound is then a failed assertion instead of a
    /// test run that never finishes.
    async fn must_bound_itself<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(30), future)
            .await
            .expect("the build stream must bound this itself rather than wait forever")
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

    /// A content-store request, whose fields the blob path stands in for.
    fn image_transfer() -> ImageTransfer {
        ImageTransfer {
            id: "request-1".to_string(),
            direction: TransferDirection::Outof as i32,
            tag: String::new(),
            descriptor: None,
            data: Vec::new(),
            complete: false,
            metadata: HashMap::new(),
        }
    }

    /// A blob served straight out of a file, so the chunking can be exercised
    /// without a populated content store.
    fn blob_range<'a>(
        path: &'a Path,
        size: u64,
        offset: u64,
        requested: Option<u64>,
    ) -> BlobRange<'a> {
        BlobRange {
            digest: "sha256:layer",
            path,
            method: CONTENT_READER_AT_METHOD,
            size,
            offset,
            requested,
        }
    }

    /// Collect the `ImageTransfer` replies a handler queued.
    fn drain_image(rx: &mut tokio::sync::mpsc::Receiver<ClientStream>) -> Vec<ImageTransfer> {
        rx.close();
        let mut replies = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Some(client_stream::PacketType::ImageTransfer(transfer)) = msg.packet_type {
                replies.push(transfer);
            }
        }
        replies
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
        let request = build_transfer(
            &[("stage", "fssync"), ("method", "Walk"), ("mode", "tar")],
            ".",
        );

        run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path()))
            .expect("a tar walk must succeed");
        let replies = drain(&mut rx);

        assert!(replies.len() >= 2, "expected a hash packet and an archive");
        let (_, hash_packet) = &replies[0];
        assert!(
            hash_packet.metadata.contains_key("hash"),
            "the first packet must carry the checksum: {:?}",
            hash_packet.metadata
        );
        assert!(
            hash_packet.data.is_empty(),
            "the hash packet carries no data"
        );
        assert!(!hash_packet.complete);

        assert!(
            replies
                .iter()
                .all(|(_, p)| !p.metadata.contains_key("mode")),
            "no reply may advertise a transfer mode the shim does not implement"
        );
        assert!(
            replies[1..]
                .iter()
                .all(|(_, p)| !p.metadata.contains_key("hash")),
            "only the first packet may carry a hash, or the rest are read as more hashes"
        );

        let (_, last) = replies.last().expect("at least one packet");
        assert!(
            last.complete,
            "the final archive packet must set `complete`"
        );
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
            .map(|e| {
                e.expect("entry")
                    .path()
                    .expect("path")
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["app.txt".to_string()]);
    }

    /// Replies are demultiplexed by the per-request id, not the build's own.
    #[test]
    fn walk_replies_echo_the_requests_routing_ids() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path())).expect("walk");

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

        let outcome = run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path()));
        assert!(
            outcome.is_err(),
            "an unsupported mode must not report success"
        );

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

        run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path())).expect("walk");
        let replies = drain(&mut rx);

        assert!(
            replies.len() >= 2,
            "a hash packet and at least one data packet"
        );
        assert!(replies.last().expect("last").1.complete);
    }

    /// `.dockerignore` reaches the client as `exclude-patterns`; a walk that
    /// ignores it ships `.git` and `target` on every build.
    #[test]
    fn walk_honours_the_exclude_patterns_dockerignore_produced() {
        let dir = context_with(&[
            ("app.txt", "payload"),
            ("target/debug/huge.bin", "lots"),
            (".git/config", "[core]"),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(
            &[
                ("method", "Walk"),
                ("mode", "tar"),
                ("exclude-patterns", "target,.git"),
            ],
            ".",
        );

        run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path())).expect("walk");
        let replies = drain(&mut rx);

        let archive: Vec<u8> = replies[1..]
            .iter()
            .flat_map(|(_, p)| p.data.clone())
            .collect();
        let names: Vec<String> = tar::Archive::new(archive.as_slice())
            .entries()
            .expect("entries")
            .map(|e| {
                e.expect("entry")
                    .path()
                    .expect("path")
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["app.txt".to_string()]);
    }

    /// A context larger than one packet must arrive as several bounded
    /// packets, with `complete` only on the last one.
    #[test]
    fn walk_sends_a_large_context_as_bounded_chunks() {
        let payload = "0123456789abcdef".repeat(fssync::CONTEXT_CHUNK_SIZE / 8);
        let dir = context_with(&[("big.bin", payload.as_str())]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        run(handle_walk(&request, &sink(&tx), REPLY_ID, dir.path())).expect("walk");
        let replies = drain(&mut rx);

        let data = &replies[1..];
        assert!(data.len() > 1, "a large context must span several packets");
        assert!(
            data.iter()
                .all(|(_, p)| p.data.len() <= fssync::CONTEXT_CHUNK_SIZE),
            "no packet may exceed the transfer limit"
        );
        assert_eq!(
            data.iter().filter(|(_, p)| p.complete).count(),
            1,
            "`complete` ends the transfer, so exactly one packet may set it"
        );
        assert!(data.last().expect("last").1.complete);

        let archive: Vec<u8> = data.iter().flat_map(|(_, p)| p.data.clone()).collect();
        let (_, hash_packet) = &replies[0];
        let expected = fssync::build_context_tar(dir.path(), &fssync::ContextFilter::default())
            .expect("reference archive");
        assert_eq!(
            archive, expected.1,
            "the packets must reassemble the archive"
        );
        assert_eq!(
            hash_packet.metadata.get("hash").map(String::as_str),
            Some(expected.0.as_str()),
            "the checksum must name the archive that was actually sent"
        );
    }

    /// The checksum is announced before the archive is produced a second time
    /// to send it, so a context rewritten in between would hand the shim bytes
    /// its hash does not describe — and the shim caches its unpacked context by
    /// that hash, so a later stable build would silently reuse the wrong tree.
    /// The transfer must fail instead of completing.
    #[test]
    fn walk_fails_when_the_sent_archive_does_not_match_the_announced_checksum() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");
        let entries = fssync::collect_context(dir.path(), &fssync::ContextFilter::default())
            .expect("context walk");
        let stale = "0".repeat(64);

        let outcome = run(stream_walk_archive(
            entries,
            &stale,
            &sink(&tx),
            REPLY_ID,
            &request,
        ));
        assert!(
            outcome.is_err(),
            "an archive that does not match its checksum must not report success"
        );

        let replies = drain(&mut rx);
        assert!(
            replies
                .iter()
                .any(|(_, p)| p.metadata.contains_key("error")),
            "the shim only stops waiting when it sees an `error` key: {replies:?}"
        );
        assert!(
            !replies
                .iter()
                .any(|(_, p)| p.complete && !p.metadata.contains_key("error")),
            "a mismatched transfer must never be completed"
        );
    }

    // ---- the build stream is bidirectional, so a send that cannot be
    // delivered has to fail the way a receive that never arrives does ----

    /// A packet that the builder never accepts must fail on the idle budget,
    /// not park forever. This is the both-sides-blocked shape: the shim stops
    /// draining because it is itself blocked handing us a packet the receive
    /// loop is not reading while this send is outstanding, so neither side ever
    /// moves and the sink's own deadline is the only thing that can break it.
    #[test]
    fn a_send_the_builder_never_accepts_fails_on_the_idle_budget() {
        let outcome = run(async {
            // Capacity one, already full and never drained: the next send can
            // only complete once the builder consumes, which it never does.
            let (tx, _rx) = tokio::sync::mpsc::channel::<ClientStream>(1);
            tx.send(ClientStream {
                build_id: REPLY_ID.to_string(),
                packet_type: None,
            })
            .await
            .expect("the first packet fits");

            must_bound_itself(impatient_sink(&tx).send(
                ClientStream {
                    build_id: REPLY_ID.to_string(),
                    packet_type: None,
                },
                "an fssync reply",
            ))
            .await
        });

        let message = outcome
            .expect_err("a send that cannot be delivered must not wait forever")
            .to_string();
        assert!(
            message.contains("stopped accepting packets"),
            "the error must name the stall, got: {message}"
        );
        assert!(
            message.contains("an fssync reply"),
            "the error must name what could not be sent, got: {message}"
        );
    }

    /// The receiving half going away is fatal too: the builder is waiting on
    /// this packet and has no deadline of its own, so a swallowed error hangs
    /// the build with nothing to report.
    #[test]
    fn a_send_to_a_closed_stream_is_reported_rather_than_swallowed() {
        let outcome = run(async {
            let (tx, rx) = tokio::sync::mpsc::channel::<ClientStream>(1);
            drop(rx);
            sink(&tx)
                .send(
                    ClientStream {
                        build_id: REPLY_ID.to_string(),
                        packet_type: None,
                    },
                    "the resolver reply",
                )
                .await
        });

        let message = outcome
            .expect_err("a closed stream must surface as an error")
            .to_string();
        assert!(
            message.contains("the resolver reply"),
            "the error must name what could not be sent, got: {message}"
        );
    }

    /// The walk streams the archive through the same bounded send, so a shim
    /// that stops reading fails the transfer instead of pinning the receive
    /// loop and a blocking-pool thread for the rest of the build.
    ///
    /// The context is several chunks so the loop is genuinely mid-transfer when
    /// the sink stops accepting, which is the case that holds the receive loop
    /// longest.
    #[test]
    fn a_walk_whose_packets_are_never_drained_fails_on_the_idle_budget() {
        let payload = "0123456789abcdef".repeat(fssync::CONTEXT_CHUNK_SIZE / 2);
        let dir = context_with(&[("big.bin", payload.as_str())]);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        let outcome = run(async {
            let (tx, _rx) = tokio::sync::mpsc::channel::<ClientStream>(1);
            let entries = fssync::collect_context(dir.path(), &fssync::ContextFilter::default())
                .expect("context walk");
            must_bound_itself(stream_walk_archive(
                entries,
                &"0".repeat(64),
                &impatient_sink(&tx),
                REPLY_ID,
                &request,
            ))
            .await
        });

        let message = outcome
            .expect_err("an undrained walk must not hang the build")
            .to_string();
        assert!(
            message.contains("stopped accepting packets"),
            "the error must name the stall, got: {message}"
        );
    }

    /// The producing half has to be bounded like the sending half: a context
    /// file the filesystem never answers for (a named pipe, a hung virtiofs or
    /// NFS mount) would otherwise park the walk forever with nothing reported.
    #[test]
    fn a_walk_whose_context_never_produces_a_chunk_fails_on_the_idle_budget() {
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");

        let outcome = run(async {
            let (tx, _rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            // A producer that never emits and never finishes, as a hung NFS or
            // virtiofs mount leaves it: the sender stays alive, so the receive
            // can only end on its own deadline.
            let (_chunks_tx, mut chunks_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
            let writer = tokio::task::spawn_blocking(|| Ok(String::new()));

            must_bound_itself(send_archive_chunks(
                &mut chunks_rx,
                writer,
                &"0".repeat(64),
                &impatient_sink(&tx),
                REPLY_ID,
                &request,
            ))
            .await
        });

        let failure = outcome.expect_err("a context that never produces must not hang the build");
        let WalkFailure::Reportable(e) = failure else {
            panic!("a stalled producer leaves the stream out usable, so it must be reportable");
        };
        assert!(
            e.to_string().contains("produced nothing"),
            "the error must name the stall, got: {e}"
        );
    }

    /// A failure that leaves the stream out usable is reported to the builder
    /// rather than leaving it to wait out its own idle budget. A checksum
    /// mismatch is that kind of failure — the archive was produced fine, so the
    /// sink still works — and it shares the reporting arm with the producer
    /// stall the test above pins to `Reportable`.
    #[test]
    fn a_reportable_walk_failure_is_sent_to_the_builder() {
        let dir = context_with(&[("app.txt", "payload")]);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");
        let entries = fssync::collect_context(dir.path(), &fssync::ContextFilter::default())
            .expect("context walk");

        let (outcome, replies) = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            let outcome = must_bound_itself(stream_walk_archive(
                entries,
                &"0".repeat(64),
                &sink(&tx),
                REPLY_ID,
                &request,
            ))
            .await;
            (outcome, drain(&mut rx))
        });

        assert!(outcome.is_err(), "a mismatched archive must not succeed");
        assert!(
            replies
                .iter()
                .any(|(_, p)| p.metadata.contains_key("error")),
            "the shim only stops waiting when it sees an `error` key: {replies:?}"
        );
    }

    /// When the stream out is what failed, sending an error packet on it would
    /// only spend a second idle budget on the same wedged channel and bury the
    /// failure that actually happened.
    #[test]
    fn a_walk_whose_sink_failed_reports_that_rather_than_sending_again() {
        // Several chunks, so the sink is genuinely used mid-archive rather than
        // only for the final packet.
        let payload = "0123456789abcdef".repeat(fssync::CONTEXT_CHUNK_SIZE / 2);
        let dir = context_with(&[("big.bin", payload.as_str())]);
        let request = build_transfer(&[("method", "Walk"), ("mode", "tar")], ".");
        let entries = fssync::collect_context(dir.path(), &fssync::ContextFilter::default())
            .expect("context walk");

        let outcome = run(async {
            let (tx, rx) = tokio::sync::mpsc::channel::<ClientStream>(1);
            // A closed receiver fails the send the moment it is attempted, so
            // the sink loses this race outright rather than on a timer the
            // producer could beat on a loaded machine.
            drop(rx);
            must_bound_itself(stream_walk_archive(
                entries,
                &"0".repeat(64),
                &sink(&tx),
                REPLY_ID,
                &request,
            ))
            .await
        });

        let message = outcome
            .expect_err("a dead sink must surface as an error")
            .to_string();
        assert!(
            message.contains("failed to send"),
            "the send failure itself must be reported, not a later one: {message}"
        );
    }

    // ---- a request we do not implement must be answered, not ignored: the
    // shim's receivers have no deadline of their own ----

    /// A `BuildSession` over a throwaway context, for the dispatch tests.
    fn session_at(context: &Path) -> BuildSession<'_> {
        BuildSession {
            context,
            resolved: None,
            pulled: Vec::new(),
            unanswered: HashSet::new(),
        }
    }

    #[test]
    fn an_unknown_fssync_method_is_answered_with_an_error() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("stage", "fssync"), ("method", "Truncate")], "app.txt");

        run(handle_build_transfer(
            &request,
            &sink(&tx),
            REPLY_ID,
            &mut session_at(dir.path()),
        ))
        .expect("an unimplemented method must still be answered");

        let replies = drain(&mut rx);
        assert_eq!(replies.len(), 1, "exactly one error packet");
        let (_, reply) = &replies[0];
        assert!(reply.metadata.contains_key("error"), "{:?}", reply.metadata);
        assert!(reply.complete);
    }

    /// An error reply is addressed by the stage and method it echoes, so a
    /// packet naming neither cannot be answered — and injecting a `complete`
    /// packet carrying an error into a transfer that was never a request would
    /// abort a build that was running fine.
    #[test]
    fn a_packet_that_names_no_request_is_not_answered() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        for request in [
            build_transfer(&[], "."),
            build_transfer(&[("stage", "fssync")], "."),
            build_transfer(&[("method", "Walk")], "."),
        ] {
            run(handle_build_transfer(
                &request,
                &sink(&tx),
                REPLY_ID,
                &mut session_at(dir.path()),
            ))
            .expect("an unroutable packet must not fail the build");
        }

        assert!(
            drain(&mut rx).is_empty(),
            "nothing may be injected into a transfer that named no request"
        );
    }

    /// Nothing establishes what the shim sets `complete` to on a request, and a
    /// one-shot request that carries all its parameters at once is a natural
    /// `complete: true`. Staying silent on one would cost the whole 600s idle
    /// budget and kill the build with the wrong reason, so a packet that names
    /// a request is answered whatever `complete` and `direction` say.
    #[test]
    fn a_request_is_answered_whatever_its_completion_flag_says() {
        let dir = context_with(&[("app.txt", "0123456789")]);

        for complete in [false, true] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let mut request = build_transfer(
                &[
                    ("stage", "fssync"),
                    ("method", "Read"),
                    ("offset", "0"),
                    ("length", "4"),
                ],
                "app.txt",
            );
            request.complete = complete;
            request.direction = TransferDirection::Outof as i32;

            run(handle_build_transfer(
                &request,
                &sink(&tx),
                REPLY_ID,
                &mut session_at(dir.path()),
            ))
            .expect("a request must be answered");

            let replies = drain(&mut rx);
            assert_eq!(
                replies.len(),
                1,
                "a request with complete={complete} must be answered"
            );
            assert_eq!(replies[0].1.data, b"0123");
        }
    }

    /// One line per stage and method, however many packets arrive: this shares
    /// stderr with the build's own output, which a per-packet warning on a
    /// keepalive would bury.
    #[test]
    fn unanswered_packets_are_noted_once_per_stage_and_method() {
        let mut seen = HashSet::new();
        note_unanswered_packet(&mut seen, "BuildTransfer", "fssync", "Read");
        note_unanswered_packet(&mut seen, "BuildTransfer", "fssync", "Read");
        note_unanswered_packet(&mut seen, "BuildTransfer", "fssync", "Walk");

        assert_eq!(seen.len(), 2, "a repeated pair must be noted only once");
    }

    /// This key's encoding is reverse-engineered, so the rule is conservative:
    /// report only what unambiguously names refused members. Reading any other
    /// shape as a rejection would fail a build whose load actually succeeded.
    #[test]
    fn rejected_members_are_read_only_when_they_plainly_name_members() {
        assert_eq!(
            rejected_members(br#"["a.tar","b.tar"]"#).named,
            ["a.tar", "b.tar"]
        );
        assert_eq!(
            rejected_members(br#"{"rejected":["a.tar"]}"#).named,
            ["a.tar"]
        );
        assert_eq!(
            rejected_members(br#"{"rejectedMembers":["a.tar"]}"#).named,
            ["a.tar"]
        );

        for benign in [
            &b"null"[..],
            b"[]",
            b"{}",
            br#"{"rejected":[]}"#,
            br#""""#,
            b"not json at all",
        ] {
            assert!(
                rejected_members(benign).is_empty(),
                "{:?} names no refused member, so it must not fail a load that succeeded",
                String::from_utf8_lossy(benign)
            );
        }
    }

    /// The list is taken by name. `serde_json`'s map is a `BTreeMap` here, so
    /// picking whichever array sorted first would report the *accepted* files
    /// as the refused ones and fail a load that fully succeeded.
    #[test]
    fn rejected_members_are_taken_by_key_not_by_sort_order() {
        let reply = br#"{"accepted":["app.tar"],"rejected":[]}"#;
        assert!(
            rejected_members(reply).is_empty(),
            "a load that refused nothing must not be read out of the accepted list"
        );

        let reply = br#"{"accepted":["app.tar"],"rejected":["bad.tar"]}"#;
        assert_eq!(rejected_members(reply).named, ["bad.tar"]);

        // A preferred key holding something that is not a list — the natural
        // encoding of an absent Swift optional — must fall through to the name
        // that does carry one, not shadow it into a clean load.
        let reply = br#"{"rejectedMembers":null,"rejected":["bad.tar"]}"#;
        assert_eq!(
            rejected_members(reply).named,
            ["bad.tar"],
            "a null preferred key must not hide the list beside it"
        );
    }

    /// An entry this version cannot read a filename out of is still a refused
    /// member: counting it keeps a partial refusal detectable, where dropping
    /// it would tag an image the daemon did not fully load.
    #[test]
    fn rejected_members_this_version_cannot_name_are_still_counted() {
        let reply = br#"{"rejected":[{"path":"app.tar","reason":"unsupported"}]}"#;
        let rejected = rejected_members(reply);

        assert!(rejected.named.is_empty());
        assert_eq!(rejected.unnamed, 1);
        assert!(
            !rejected.is_empty(),
            "a refusal must not read as a clean load"
        );
        assert!(
            rejected.describe().contains("1 more"),
            "{}",
            rejected.describe()
        );

        let mixed = rejected_members(br#"{"rejected":["a.tar",{"path":"b.tar"}]}"#);
        assert_eq!(mixed.named, ["a.tar"]);
        assert_eq!(mixed.unnamed, 1);
        assert!(
            mixed.describe().starts_with("a.tar, "),
            "{}",
            mixed.describe()
        );
    }

    #[test]
    fn an_unknown_build_transfer_stage_is_answered_with_an_error() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("stage", "diffcopy"), ("method", "Walk")], ".");

        run(handle_build_transfer(
            &request,
            &sink(&tx),
            REPLY_ID,
            &mut session_at(dir.path()),
        ))
        .expect("an unimplemented stage must still be answered");

        let replies = drain(&mut rx);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].1.metadata.contains_key("error"));
    }

    // ---- builder-supplied paths are confined to the build context ----

    /// The builder names the paths it wants and we hand back their contents,
    /// so an unconfined source is a read of any file this user can reach.
    #[test]
    fn a_source_outside_the_context_resolves_to_nothing() {
        let context = Path::new("/workspace/project");

        assert_eq!(
            resolve_path(context, "src/main.rs"),
            Some(context.join("src/main.rs"))
        );
        assert_eq!(
            resolve_path(context, "./app.txt"),
            Some(context.join("app.txt"))
        );
        assert_eq!(resolve_path(context, "."), Some(context.to_path_buf()));
        assert_eq!(
            resolve_path(context, "/workspace/project/src/main.rs"),
            Some(context.join("src/main.rs"))
        );

        assert_eq!(resolve_path(context, "/etc/passwd"), None);
        assert_eq!(resolve_path(context, "../../../.ssh/id_rsa"), None);
        assert_eq!(resolve_path(context, "src/../../escape"), None);
        assert_eq!(resolve_path(context, "/workspace/project/../secrets"), None);
    }

    #[test]
    fn read_refuses_a_path_outside_the_context() {
        let root = context_with(&[("context/app.txt", "payload"), ("secret.txt", "private")]);
        let context = root.path().join("context");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Read"), ("length", "64")], "../secret.txt");

        run(handle_read(&request, &sink(&tx), REPLY_ID, &context)).expect("read");

        let (_, reply) = &drain(&mut rx)[0];
        assert!(
            reply.data.is_empty() && reply.metadata.contains_key("error"),
            "a path outside the context must be refused, got: {reply:?}"
        );
    }

    /// Confining the name is not enough, because reading follows links: a
    /// symlink inside the workspace pointing out of it — one a dependency or an
    /// earlier tool left behind — would otherwise stream a private file into
    /// the VM under a path that looks perfectly confined.
    #[test]
    fn read_refuses_a_symlink_that_leaves_the_context() {
        let root = context_with(&[("context/app.txt", "payload"), ("secret.txt", "private")]);
        let context = root.path().join("context");
        std::os::unix::fs::symlink(root.path().join("secret.txt"), context.join("key"))
            .expect("symlink");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Read"), ("length", "64")], "key");

        run(handle_read(&request, &sink(&tx), REPLY_ID, &context)).expect("read");

        let (_, reply) = &drain(&mut rx)[0];
        assert!(
            reply.data.is_empty() && reply.metadata.contains_key("error"),
            "a symlink out of the context must not be followed, got: {reply:?}"
        );
        assert!(
            !reply.data.windows(7).any(|w| w == b"private"),
            "the linked file's contents must never be sent"
        );
    }

    /// A link that stays inside the context is ordinary and must still be read.
    #[test]
    fn read_follows_a_symlink_that_stays_inside_the_context() {
        let dir = context_with(&[("app.txt", "payload")]);
        std::os::unix::fs::symlink("app.txt", dir.path().join("link.txt")).expect("symlink");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Read"), ("length", "64")], "link.txt");

        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");
        assert_eq!(drain(&mut rx)[0].1.data, b"payload");
    }

    /// `Info` describes the link itself and BuildKit resolves it on its own
    /// side, exactly as fsutil sends the link rather than its target — so the
    /// read guard must not turn a symlink into an error here.
    #[test]
    fn info_still_describes_a_symlink_that_points_out_of_the_context() {
        let root = context_with(&[("context/app.txt", "payload"), ("secret.txt", "private")]);
        let context = root.path().join("context");
        std::os::unix::fs::symlink(root.path().join("secret.txt"), context.join("key"))
            .expect("symlink");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "key");

        run(handle_info(&request, &sink(&tx), REPLY_ID, &context)).expect("info");

        let (_, reply) = &drain(&mut rx)[0];
        assert!(
            !reply.metadata.contains_key("error"),
            "{:?}",
            reply.metadata
        );
        assert!(
            reply.metadata.contains_key("target"),
            "the link must be described as a link: {:?}",
            reply.metadata
        );
    }

    #[test]
    fn info_refuses_an_absolute_path_outside_the_context() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "/etc/passwd");

        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");

        let (_, reply) = &drain(&mut rx)[0];
        assert!(reply.metadata.contains_key("error"), "{:?}", reply.metadata);
        assert!(
            !reply.metadata.contains_key("size"),
            "nothing outside the context may be described"
        );
    }

    /// A directory name may hold a character a header may not. Building under
    /// one has to fail with a message rather than panic part-way through
    /// `dev up`.
    #[test]
    fn a_header_value_a_header_cannot_carry_is_reported_not_panicked_on() {
        assert!(header_value("context", "/Users/dev/project").is_ok());
        // Headers carry obs-text, so an accented path is not the problem.
        assert!(header_value("context", "/Users/josé/proj").is_ok());

        let error = header_value("context", "/Users/dev/two\nlines")
            .expect_err("a control character must not be sent as a header");
        assert!(error.to_string().contains("context"), "{error}");
        assert!(header_value("tag", "tag\rwith-return").is_err());
        assert!(header_value("tag", "tag\0nul").is_err());
    }

    /// `File.ReadAt` reads an empty reply as EOF, so an absent length must mean
    /// "some of the file" rather than none of it. This is the fssync reading of
    /// an absent length; the content store takes the same absence as the rest
    /// of the blob.
    #[test]
    fn a_read_length_never_defaults_to_nothing() {
        let asked = |pairs: &[(&str, &str)]| {
            fssync_read_length(
                requested_read_window(&metadata(pairs))
                    .expect("a readable window")
                    .length,
            )
        };

        assert_eq!(asked(&[]), MAX_REPLY_PAYLOAD);
        assert_eq!(asked(&[("length", "512")]), 512);
        // The request is taken as it was made; whether it can be answered in
        // one reply is `readable_span`'s decision.
        assert_eq!(asked(&[("length", "5000000000")]), 5_000_000_000);
        assert_eq!(asked(&[("length", "0")]), 0);
    }

    /// The fssync `Read` path reads its window through the same rule as the
    /// content store, so a number it cannot read is reported rather than
    /// quietly replaced by a default — serving byte 0 for a read that named
    /// some other position hands back the wrong bytes under a reply that
    /// claims they are the right ones.
    #[test]
    fn an_fssync_read_refuses_an_unreadable_window_rather_than_defaulting() {
        for (key, unreadable) in [("offset", "-1"), ("length", "0x1000")] {
            let dir = context_with(&[("app.txt", "0123456789")]);
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let request = build_transfer(&[("method", "Read"), (key, unreadable)], "app.txt");

            run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");

            let replies = drain(&mut rx);
            let reported = replies[0]
                .1
                .metadata
                .get("error")
                .unwrap_or_else(|| panic!("an unreadable {key} must be reported"));
            assert!(
                reported.contains(key) && reported.contains(&format!("{unreadable:?}")),
                "{reported}"
            );
            assert!(
                replies.iter().all(|(_, reply)| reply.data.is_empty()),
                "no bytes may be served for a window this client could not read"
            );
        }
    }

    /// `File.ReadAt` sets `io.EOF` when the reply is shorter than the length it
    /// asked for, so the end of the file is the one thing that may shorten one.
    /// A range the file could serve but a reply cannot is refused instead:
    /// clamping it would be read as the end of the file, and the image would
    /// carry a truncated copy with nothing said on either side.
    #[test]
    fn an_fssync_read_larger_than_one_reply_is_refused_not_cut_short() {
        // A reply must still fit inside gRPC's default message limit.
        const { assert!(MAX_REPLY_PAYLOAD < 4 * 1024 * 1024) };

        // Short of the end of the file: complete however small.
        assert_eq!(readable_span(512, 4096).expect("a servable span"), 512);
        // Clamped by the file's end, which is EOF rather than truncation.
        assert_eq!(readable_span(4096, 512).expect("a servable span"), 512);
        assert_eq!(
            readable_span(MAX_REPLY_PAYLOAD, MAX_REPLY_PAYLOAD).expect("a servable span"),
            MAX_REPLY_PAYLOAD as usize
        );

        // An asset larger than one reply, asked for whole.
        let asset = 8 * 1024 * 1024;
        let refused = readable_span(asset, asset).expect_err("a span one reply cannot carry");
        assert!(refused.to_string().contains("more than the"), "{refused}");
        // The file's end still shortens it, so the tail of that same asset is
        // servable: it is capacity that is refused, not the file's size.
        assert_eq!(
            readable_span(asset, 4096).expect("the tail of an oversized asset"),
            4096
        );
    }

    /// The refusal has to reach the builder as an fssync error, not as a
    /// silently short reply the shim would take for the end of the file.
    #[test]
    fn an_fssync_read_reports_a_span_one_reply_cannot_carry() {
        let dir = tempfile::tempdir().expect("temp context");
        let asset = (MAX_REPLY_PAYLOAD as usize) + 4096;
        std::fs::write(dir.path().join("asset.bin"), vec![7u8; asset]).expect("asset");

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(
            &[("method", "Read"), ("length", &asset.to_string())],
            "asset.bin",
        );
        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");

        let replies = drain(&mut rx);
        let reported = replies[0]
            .1
            .metadata
            .get("error")
            .expect("a span one reply cannot carry must be reported");
        assert!(reported.contains("more than the"), "{reported}");
        assert!(
            replies.iter().all(|(_, reply)| reply.data.is_empty()),
            "no partial payload may be handed over for a refused read"
        );
    }

    /// A named pipe in the context blocks its opener until a writer appears,
    /// and nothing above this call has a deadline — so the type has to be
    /// refused at the open rather than discovered by hanging `dev up`.
    #[test]
    fn an_fssync_read_of_a_named_pipe_is_refused_rather_than_blocked_on() {
        let dir = tempfile::tempdir().expect("temp context");
        let fifo = std::ffi::CString::new(
            dir.path()
                .join("dev-server.fifo")
                .to_string_lossy()
                .as_bytes(),
        )
        .expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0, "mkfifo");

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Read")], "dev-server.fifo");
        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");

        let replies = drain(&mut rx);
        let reported = replies[0]
            .1
            .metadata
            .get("error")
            .expect("a path that is not a regular file must be reported");
        assert!(
            reported.contains("stopped being a regular file"),
            "{reported}"
        );
    }

    /// `Info` resolves the directory chain before it stats, so a link left in
    /// the context cannot be walked *through* to describe a host file. The
    /// final component is still described as the link it is — that is what the
    /// shim asked for — which is why the chain is confined and it is not.
    #[test]
    fn an_fssync_info_refuses_a_path_that_leaves_the_context_through_a_link() {
        let dir = tempfile::tempdir().expect("temp context");
        let outside = tempfile::tempdir().expect("temp outside");
        std::fs::write(outside.path().join("secret"), "not the build's").expect("outside file");
        std::fs::create_dir(dir.path().join("deploy")).expect("deploy");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("deploy/etc")).expect("symlink");

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "deploy/etc/secret");
        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");

        let replies = drain(&mut rx);
        let reported = replies[0]
            .1
            .metadata
            .get("error")
            .expect("a path resolving outside the context must be reported");
        assert!(reported.contains(UNCONFINED), "{reported}");
        assert!(
            !replies[0].1.metadata.contains_key("size"),
            "no metadata of a host file may be handed over"
        );
    }

    /// The link itself is still described, because BuildKit resolves links on
    /// its own side and expects to be told the link rather than its target.
    #[test]
    fn an_fssync_info_still_describes_a_link_in_the_context_as_a_link() {
        let dir = tempfile::tempdir().expect("temp context");
        std::fs::write(dir.path().join("app.txt"), "payload").expect("app");
        std::os::unix::fs::symlink("app.txt", dir.path().join("link.txt")).expect("symlink");

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "link.txt");
        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");

        let replies = drain(&mut rx);
        let metadata = &replies[0].1.metadata;
        assert!(!metadata.contains_key("error"), "{metadata:?}");
        assert_eq!(
            metadata.get("target").map(String::as_str),
            Some("app.txt"),
            "a link must be described as a link, not as its target"
        );
    }

    /// One request, one reply — so a range larger than a reply cannot be
    /// answered at all. Cutting it short and marking it done would be taken for
    /// the whole range: `fetchChunk` caches a short read as a chunk of that
    /// length with no refill, and the prefetcher then compacts the next chunk
    /// against it. The verified protocol never asks this (1 MiB windows), which
    /// is exactly why refusing costs nothing and a silent truncation would be
    /// undiagnosable.
    #[test]
    fn a_blob_read_larger_than_one_reply_is_refused_not_cut_short() {
        let layer = (MAX_REPLY_PAYLOAD as usize) * 2 + 1024;
        let content: Vec<u8> = (0..layer).map(|i| (i % 251) as u8).collect();
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, &content).expect("blob");

        let request = image_transfer();
        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, layer as u64, 0, Some(layer as u64)),
            )
            .await
            .expect("the refusal must be reported, not returned as an error here");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        let reply = &replies[0];
        assert!(
            reply.metadata.contains_key("error"),
            "a range one reply cannot carry must be refused: {:?}",
            reply.metadata
        );
        assert!(
            reply.data.is_empty(),
            "no partial payload may be handed over for a refused range"
        );
        assert!(
            reply.metadata["error"].contains("more than the"),
            "{}",
            reply.metadata["error"]
        );
    }

    /// The protocol as verified: the prefetcher walks an oversized layer in
    /// `ChunkSize` windows, one packet each, and the windows reassemble into
    /// the layer. This is the end-to-end claim the single-packet design rests
    /// on, driven here rather than left to the source reading.
    #[test]
    fn an_oversized_layer_is_served_as_a_sequence_of_windowed_reads() {
        const CHUNK: usize = 1 << 20;
        let layer = CHUNK * 3 + 4096;
        let content: Vec<u8> = (0..layer).map(|i| (i % 251) as u8).collect();
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, &content).expect("blob");

        let served = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            let request = image_transfer();
            let mut at = 0u64;
            // The `ChunkSize`-at-a-time walk `prefetcher.fetchChunk` performs,
            // stopping at the end of the layer. A window starting at or past
            // EOF is not reached here; `a_blob_read_past_the_end_completes_empty`
            // covers that one on its own.
            loop {
                send_blob_range(
                    &sink(&tx),
                    REPLY_ID,
                    &request,
                    &blob_range(&blob, layer as u64, at, Some(CHUNK as u64)),
                )
                .await
                .expect("each window must be servable");
                at += CHUNK as u64;
                if at >= layer as u64 {
                    break;
                }
            }
            drain_image(&mut rx)
        });

        assert_eq!(served.len(), 4, "a 3-and-a-bit chunk layer takes 4 windows");
        assert!(
            served.iter().all(|p| p.complete),
            "every windowed read is answered whole, so each reply completes"
        );
        assert!(
            served.iter().all(|p| !p.metadata.contains_key("error")),
            "no window of a healthy layer may be refused"
        );

        let reassembled: Vec<u8> = served.iter().flat_map(|p| p.data.clone()).collect();
        assert_eq!(
            reassembled, content,
            "the windows must reassemble into the layer"
        );

        let mut expected = 0usize;
        for packet in &served {
            assert_eq!(
                packet.metadata.get("offset").map(String::as_str),
                Some(expected.to_string().as_str())
            );
            assert_eq!(
                packet.metadata.get("size").map(String::as_str),
                Some(layer.to_string().as_str()),
                "every reply reports the whole size, so the caller knows to read on"
            );
            expected += packet.data.len();
        }
        assert_eq!(expected, layer);
    }

    /// A read that begins part-way through is answered from that offset, which
    /// is how the prefetcher walks a layer it asked for a chunk at a time.
    #[test]
    fn a_blob_read_at_an_offset_answers_from_there() {
        let layer = (MAX_REPLY_PAYLOAD as usize) + 4096;
        let content: Vec<u8> = (0..layer).map(|i| (i % 251) as u8).collect();
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, &content).expect("blob");

        let at = MAX_REPLY_PAYLOAD;
        let request = image_transfer();
        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(8);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, layer as u64, at, Some(1 << 20)),
            )
            .await
            .expect("a windowed read must be servable");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].data, content[at as usize..]);
        assert_eq!(
            replies[0].metadata.get("offset").map(String::as_str),
            Some(at.to_string().as_str())
        );
    }

    /// `ReaderAt::init` probes with length 0 purely to learn the size, and
    /// reads an empty payload as EOF — so the probe must still get exactly one
    /// completed reply rather than nothing.
    #[test]
    fn a_zero_length_blob_read_answers_once_with_no_payload() {
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, b"0123456789").expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(8);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, 10, 0, Some(0)),
            )
            .await
            .expect("a size probe must be answered");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        assert!(replies[0].data.is_empty());
        assert!(replies[0].complete);
        assert_eq!(
            replies[0].metadata.get("size").map(String::as_str),
            Some("10")
        );
    }

    /// A `ReaderAt` that names no length asks for the rest of the blob, and a
    /// rest that does not fit in one reply is refused on the same terms as a
    /// named length that does not. Clamping instead would hand back a full
    /// reply the caller cannot tell from a complete answer.
    #[test]
    fn a_lengthless_blob_read_is_refused_when_the_rest_will_not_fit() {
        let layer = (MAX_REPLY_PAYLOAD as usize) + 4096;
        let content: Vec<u8> = (0..layer).map(|i| (i % 251) as u8).collect();
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, &content).expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, layer as u64, 0, None),
            )
            .await
            .expect("the refusal must be reported, not returned as an error here");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].metadata["error"].contains("more than the"),
            "{}",
            replies[0].metadata["error"]
        );
        assert!(
            replies[0].data.is_empty(),
            "no partial payload may be handed over for a refused range"
        );
    }

    /// The rest of a blob that *does* fit is still answered whole, with the
    /// size the caller reads on from — the refusal above is about capacity,
    /// not about naming no length.
    #[test]
    fn a_lengthless_blob_read_that_fits_is_answered_whole() {
        let layer = (MAX_REPLY_PAYLOAD as usize) - 4096;
        let content: Vec<u8> = (0..layer).map(|i| (i % 251) as u8).collect();
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, &content).expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(64);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, layer as u64, 0, None),
            )
            .await
            .expect("a lengthless read that fits must be answered");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        assert!(replies[0].complete);
        assert_eq!(replies[0].data, content);
        assert_eq!(
            replies[0].metadata.get("size").map(String::as_str),
            Some(layer.to_string().as_str())
        );
    }

    /// The content store said one size and the file is another: the blob was
    /// replaced or collected under us. `ReadAt` checks `metadata["error"]`
    /// before it looks at the payload, so the disagreement is reported rather
    /// than answered with bytes whose digest cannot match.
    #[test]
    fn a_blob_shorter_than_the_store_says_is_reported_not_answered() {
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, b"0123456789").expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(8);
            // The store reports 4 KiB; the file holds ten bytes.
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, 4096, 0, Some(4096)),
            )
            .await
            .expect("the shortfall must be reported, not returned as an error here");
            drain_image(&mut rx)
        });

        let reported = replies
            .iter()
            .find(|p| p.metadata.contains_key("error"))
            .expect("a blob shorter than the store said must be reported");
        assert!(
            reported.metadata["error"].contains("4086 bytes short of the 4096"),
            "{}",
            reported.metadata["error"]
        );
        assert!(
            !replies
                .iter()
                .any(|p| !p.data.is_empty() && !p.metadata.contains_key("error")),
            "no payload may be handed over once the blob is known to be short"
        );
    }

    /// The figure is the shortfall against what this read asked for, not
    /// against the whole of what is left of the blob. A windowed read that got
    /// half its chunk is short by half a chunk; reporting it as short by the
    /// rest of a multi-gigabyte layer names a number the caller never asked
    /// about.
    #[test]
    fn a_short_blob_is_measured_against_what_the_read_asked_for() {
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, b"0123456789").expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(8);
            // The store reports 4 KiB, this read wants 1 KiB of it, and the
            // file holds ten bytes: short by 1014, not by 4086.
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, 4096, 0, Some(1024)),
            )
            .await
            .expect("the shortfall must be reported, not returned as an error here");
            drain_image(&mut rx)
        });

        let reported = replies
            .iter()
            .find(|p| p.metadata.contains_key("error"))
            .expect("a blob shorter than the store said must be reported");
        assert!(
            reported.metadata["error"].contains("1014 bytes short of the 1024"),
            "{}",
            reported.metadata["error"]
        );
    }

    /// A numeric field this client cannot read is not the same as one that
    /// named none. Folding the two together would answer a windowed read with
    /// a default it never asked for — a length it did not want, or, worse, the
    /// bytes at offset zero when it named some other position.
    #[test]
    fn an_unreadable_request_number_is_kept_apart_from_an_absent_one() {
        assert_eq!(
            requested_read_window(&metadata(&[("offset", "4096"), ("length", "1024")])),
            Ok(ReadWindow {
                offset: 4096,
                length: Some(1024)
            })
        );
        assert_eq!(
            requested_read_window(&metadata(&[("offset", "0"), ("length", "0")])),
            Ok(ReadWindow {
                offset: 0,
                length: Some(0)
            }),
            "the size probe names zero for both, and it must stay named"
        );
        assert_eq!(
            requested_read_window(&metadata(&[])),
            Ok(ReadWindow {
                offset: 0,
                length: None
            }),
            "naming neither is a whole-blob read from the start, not an error"
        );

        // `-1` matters for offset in particular: the shim formats a signed
        // int64, so a negative would arrive as text `u64` cannot read.
        for unreadable in ["0x1000", "4096b", "", "-1", "18446744073709551616"] {
            for key in ["length", "offset"] {
                let refusal = requested_read_window(&metadata(&[(key, unreadable)]))
                    .expect_err("a window this client cannot read must be refused, not guessed");
                // Quoted, so the empty case asserts the message names it too
                // rather than passing on `contains("")` being always true.
                assert!(
                    refusal.contains(key) && refusal.contains(&format!("{unreadable:?}")),
                    "{refusal:?} should name the {key} it could not read"
                );
            }
        }
    }

    /// A read that starts past the end of the blob is EOF, not an error.
    #[test]
    fn a_blob_read_past_the_end_completes_empty() {
        let store = tempfile::tempdir().expect("temp store");
        let blob = store.path().join("layer");
        std::fs::write(&blob, b"0123456789").expect("blob");
        let request = image_transfer();

        let replies = run(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientStream>(8);
            send_blob_range(
                &sink(&tx),
                REPLY_ID,
                &request,
                &blob_range(&blob, 10, 64, Some(4096)),
            )
            .await
            .expect("a read past the end must be answered");
            drain_image(&mut rx)
        });

        assert_eq!(replies.len(), 1);
        assert!(replies[0].data.is_empty());
        assert!(replies[0].complete);
    }

    /// `pkg/fileutils/file_info.go` reads these out of the metadata map and
    /// silently substitutes zero for anything absent, so a JSON body in `data`
    /// made every file look empty.
    #[test]
    fn info_answers_in_metadata_rather_than_a_json_body() {
        let dir = context_with(&[("app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "app.txt");

        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");
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
        assert!(
            modified.ends_with('Z') && modified.contains('T'),
            "{modified}"
        );
    }

    #[test]
    fn info_flags_directories_so_the_walk_can_recurse() {
        let dir = context_with(&[("src/app.txt", "payload")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], "src");

        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");
        assert!(drain(&mut rx)[0].1.is_directory);
    }

    /// BuildKit probes for `.dockerignore` on every build; a missing path has
    /// to come back as an error rather than silently as an empty file.
    #[test]
    fn info_reports_a_missing_path_instead_of_pretending_it_is_empty() {
        let dir = tempfile::tempdir().expect("temp context");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Info")], ".dockerignore");

        run(handle_info(&request, &sink(&tx), REPLY_ID, dir.path())).expect("info");
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

        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");
        assert_eq!(drain(&mut rx)[0].1.data, b"234");
    }

    /// `content::read_range` treats length 0 as "nothing", and the shim reads
    /// an empty payload as EOF — so defaulting a missing `length` to zero
    /// would build an image whose files are all empty instead of failing.
    #[test]
    fn read_without_a_length_falls_back_to_the_rest_of_the_file() {
        let dir = context_with(&[("app.txt", "0123456789")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(&[("method", "Read"), ("offset", "4")], "app.txt");

        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");
        assert_eq!(
            drain(&mut rx)[0].1.data,
            b"456789",
            "an absent length must read to the end, not read as EOF"
        );
    }

    #[test]
    fn read_past_the_end_of_a_file_comes_back_empty() {
        let dir = context_with(&[("app.txt", "0123456789")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let request = build_transfer(
            &[("method", "Read"), ("offset", "99"), ("length", "8")],
            "app.txt",
        );

        run(handle_read(&request, &sink(&tx), REPLY_ID, dir.path())).expect("read");
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
