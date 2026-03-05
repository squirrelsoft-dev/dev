//! gRPC tunneling protocol for session stream using h2
//!
//! BuildKit establishes an HTTP/2 connection inside the bidirectional session stream.
//! We use the h2 crate to handle the HTTP/2 server protocol.

use crate::error::{Error, Result};
use bytes::Bytes;
use filemode::{UnixMode, GoFileMode};
use h2::server::{self, SendResponse};
use http::{Request, Response, StatusCode};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use prost::Message as ProstMessage;

use crate::proto::moby::buildkit::v1::BytesMessage;
use super::{FileSyncServer, AuthServer, SecretsServer};

/// Stream multiplexer for handling gRPC tunneled through session
pub struct GrpcTunnel {
    file_sync: Option<FileSyncServer>,
    auth: Option<AuthServer>,
    secrets: Option<SecretsServer>,
}

impl GrpcTunnel {
    /// Create a new gRPC tunnel
    pub fn new(
        _response_tx: mpsc::Sender<BytesMessage>,
        file_sync: Option<FileSyncServer>,
        auth: Option<AuthServer>,
        secrets: Option<SecretsServer>,
    ) -> Self {
        Self {
            file_sync,
            auth,
            secrets,
        }
    }

    /// Start HTTP/2 server over the session stream
    pub async fn serve(
        self,
        inbound_rx: mpsc::Receiver<BytesMessage>,
        outbound_tx: mpsc::Sender<BytesMessage>,
    ) -> Result<()> {
        let tunnel = Arc::new(self);

        // Create a wrapper that implements AsyncRead + AsyncWrite
        let stream = MessageStream::new(inbound_rx, outbound_tx);

        // Start HTTP/2 server
        let mut h2_conn = server::handshake(stream).await
            .map_err(|e| Error::Http2Handshake { source: e })?;

        tracing::info!("HTTP/2 server started in session tunnel");

        // Accept incoming HTTP/2 streams
        while let Some(result) = h2_conn.accept().await {
            let (request, respond) = result.map_err(|e| Error::Http2Stream { source: e })?;
            let tunnel_ref = Arc::clone(&tunnel);

            tokio::spawn(async move {
                if let Err(e) = tunnel_ref.handle_request(request, respond).await {
                    tracing::error!("Failed to handle gRPC request: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Handle a single gRPC request
    async fn handle_request(
        &self,
        req: Request<h2::RecvStream>,
        respond: SendResponse<Bytes>,
    ) -> Result<()> {
        let method = req.uri().path().to_string();
        tracing::info!("Received gRPC call: {}", method);

        // Debug: print all request headers
        eprintln!("\n=== Request Headers for {} ===", method);
        for (name, value) in req.headers() {
            if let Ok(v) = value.to_str() {
                eprintln!("  {}: {}", name, v);
            }
        }

        // Extract dir-name header before consuming req
        let dir_name = req.headers()
            .get("dir-name")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Extract followpaths header (can have multiple values)
        let followpaths: Vec<String> = req.headers()
            .get_all("followpaths")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect();

        let body = req.into_body();

        // Dispatch to appropriate service
        match method.as_str() {
            "/grpc.health.v1.Health/Check" => {
                // Read request body for unary RPC
                let payload = Self::read_unary_request(body).await?;
                let response_payload = self.handle_health_check(payload).await?;
                self.send_success_response(respond, response_payload).await
            }
            "/moby.filesync.v1.FileSync/DiffCopy" => {
                // DiffCopy is a bidirectional streaming RPC - pass the stream
                self.handle_file_sync_diff_copy_stream(body, respond, dir_name, followpaths).await
            }
            "/moby.filesync.v1.Auth/GetTokenAuthority" => {
                // Token-based auth not supported - return error to make BuildKit fall back
                // BuildKit requires either a valid pubkey or error to properly fallback to Credentials
                tracing::info!("Auth.GetTokenAuthority called - returning not implemented");
                self.send_error_response(respond, "Token auth not implemented").await
            }
            "/moby.filesync.v1.Auth/Credentials" => {
                let payload = Self::read_unary_request(body).await?;
                let response_payload = self.handle_auth_credentials(payload).await?;
                self.send_success_response(respond, response_payload).await
            }
            "/moby.filesync.v1.Auth/FetchToken" => {
                let payload = Self::read_unary_request(body).await?;
                let response_payload = self.handle_auth_fetch_token(payload).await?;
                self.send_success_response(respond, response_payload).await
            }
            "/moby.buildkit.secrets.v1.Secrets/GetSecret" => {
                let payload = Self::read_unary_request(body).await?;
                let response_payload = self.handle_secrets_get_secret(payload).await?;
                self.send_success_response(respond, response_payload).await
            }
            _ => {
                tracing::warn!("Unknown gRPC method: {}", method);
                self.send_error_response(respond, "Unimplemented").await
            }
        }
    }

    /// Read complete request body for unary RPC
    async fn read_unary_request(mut body: h2::RecvStream) -> Result<Bytes> {
        let mut request_data = Vec::new();

        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(|e| Error::Http2Stream { source: e })?;
            request_data.extend_from_slice(&chunk);
            let _ = body.flow_control().release_capacity(chunk.len());
        }

        // Skip the 5-byte gRPC prefix (1 byte compression + 4 bytes length)
        let payload = if request_data.len() > 5 {
            Bytes::copy_from_slice(&request_data[5..])
        } else {
            Bytes::new()
        };

        Ok(payload)
    }

    /// Send successful gRPC response
    async fn send_success_response(
        &self,
        mut respond: SendResponse<Bytes>,
        payload: Bytes,
    ) -> Result<()> {
        // Build gRPC response headers (without grpc-status - that goes in trailers)
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/grpc")
            .body(())
            .unwrap();

        let mut send_stream = respond.send_response(response, false)
            .map_err(|e| Error::Http2Stream { source: e })?;

        // Send response with gRPC framing (5-byte prefix)
        let mut framed = Vec::new();
        framed.push(0); // No compression
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);

        send_stream.send_data(Bytes::from(framed), false)
            .map_err(|e| Error::Http2Stream { source: e })?;

        // Send trailers with grpc-status
        let trailers = Response::builder()
            .header("grpc-status", "0")
            .body(())
            .unwrap();

        send_stream.send_trailers(trailers.headers().clone())
            .map_err(|e| Error::Http2Stream { source: e })?;

        Ok(())
    }

    /// Send error gRPC response
    async fn send_error_response(
        &self,
        mut respond: SendResponse<Bytes>,
        message: &str,
    ) -> Result<()> {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/grpc")
            .header("grpc-status", "12") // UNIMPLEMENTED
            .header("grpc-message", message)
            .body(())
            .unwrap();

        respond.send_response(response, true)
            .map_err(|e| Error::Http2Stream { source: e })?;

        Ok(())
    }

    /// Handle FileSync.DiffCopy streaming request
    async fn handle_file_sync_diff_copy_stream(
        &self,
        mut request_stream: h2::RecvStream,
        mut respond: SendResponse<Bytes>,
        dir_name: Option<String>,
        followpaths: Vec<String>,
    ) -> Result<()> {
        use crate::proto::fsutil::types::{Packet, packet::PacketType};
        use prost::Message as ProstMessage;

        use std::sync::atomic::{AtomicU32, Ordering};
        static CALL_COUNTER: AtomicU32 = AtomicU32::new(0);
        let call_id = CALL_COUNTER.fetch_add(1, Ordering::SeqCst);

        tracing::info!("handle_file_sync_diff_copy_stream called (call #{}, dir_name: {:?}, followpaths: {:?})", call_id, dir_name, followpaths);
        eprintln!("\n========== DiffCopy Call #{} (dir_name: {:?}, followpaths: {:?}) ==========", call_id, dir_name, followpaths);

        let file_sync = match &self.file_sync {
            Some(fs) => fs,
            None => {
                tracing::error!("FileSync not available");
                return self.send_error_response(respond, "FileSync not available").await;
            }
        };

        tracing::info!("FileSync.DiffCopy streaming started (call #{})", call_id);

        // Build response headers
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/grpc")
            .body(())
            .unwrap();

        let mut send_stream = respond.send_response(response, false)
            .map_err(|e| Error::Http2Stream { source: e })?;

        tracing::info!("Sent response headers for DiffCopy");

        // Get the root path from FileSyncServer
        let root_path = file_sync.get_root_path();
        tracing::info!("Starting to send STAT packets from: {} (call #{})", root_path.display(), call_id);
        eprintln!("Root path: {}, is_dir: {}", root_path.display(), root_path.is_dir());

        // Determine what to send based on dir_name header
        // BuildKit sends "dockerfile" when it only wants the Dockerfile file
        // Otherwise it wants the entire context
        use std::collections::HashMap;
        let mut file_map = HashMap::new();
        let mut id_counter = 0u32;

        let send_only_dockerfile = dir_name.as_deref() == Some("dockerfile");

        if send_only_dockerfile {
            // BuildKit only wants the Dockerfile - determine actual filename from followpaths
            // When using custom dockerfile, BuildKit sends followpaths like ["Custom.Dockerfile", ...]
            let dockerfile_name = if !followpaths.is_empty() && followpaths[0].ends_with(".Dockerfile") {
                // Custom dockerfile name
                followpaths[0].clone()
            } else {
                // Default to "Dockerfile"
                "Dockerfile".to_string()
            };

            eprintln!("BuildKit requested 'dockerfile' - sending only {}", dockerfile_name);
            use crate::proto::fsutil::types::{Packet, packet::PacketType, Stat};

            let dockerfile_path = root_path.join(&dockerfile_name);
            if !dockerfile_path.exists() {
                tracing::error!("{} not found at {}", dockerfile_name, dockerfile_path.display());
                let trailers = Response::builder()
                    .header("grpc-status", "2")
                    .header("grpc-message", format!("{} not found", dockerfile_name))
                    .body(())
                    .unwrap();
                let _ = send_stream.send_trailers(trailers.headers().clone());
                return Err(Error::PathNotFound(dockerfile_path.clone()));
            }

            let metadata = tokio::fs::metadata(&dockerfile_path).await?;

            let mut stat = Stat {
                path: dockerfile_name.clone(),
                mode: 0,
                uid: 0,
                gid: 0,
                size: metadata.len() as i64,
                mod_time: 0,
                linkname: String::new(),
                devmajor: 0,
                devminor: 0,
                xattrs: std::collections::HashMap::new(),
            };

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let unix_mode = metadata.permissions().mode();
                stat.mode = GoFileMode::from(UnixMode::from(unix_mode)).as_u32();
            }

            #[cfg(not(unix))]
            {
                stat.mode = 0o644;  // Regular file in Go FileMode format (just permissions)
            }

            let mode = stat.mode;
            let stat_packet = Packet {
                r#type: PacketType::PacketStat as i32,
                stat: Some(stat),
                id: 0,
                data: vec![],
            };

            eprintln!("DFS: Sending STAT #0: {} (FILE, mode: 0o{:o})", dockerfile_name, mode);
            Self::send_grpc_packet(&mut send_stream, &stat_packet).await?;

            // Store in file map
            file_map.insert(0, dockerfile_path);
        } else {
            // BuildKit wants the full context - send tree using depth-first traversal
            // If followpaths is specified, only send those files and their parent directories
            // fsutil requires files in depth-first order with entries sorted alphabetically within each directory
            if followpaths.is_empty() {
                eprintln!("BuildKit requested full context - sending entire directory tree");
            } else {
                eprintln!("BuildKit requested filtered context - followpaths: {:?}", followpaths);
            }

            if let Err(e) = Self::send_stat_packets_dfs(
                root_path.clone(),
                String::new(),
                &mut send_stream,
                &mut file_map,
                &mut id_counter,
                if followpaths.is_empty() { None } else { Some(&followpaths) },
            ).await {
                tracing::error!("Error sending STAT packets: {}", e);
                let trailers = Response::builder()
                    .header("grpc-status", "2")
                    .header("grpc-message", e.to_string())
                    .body(())
                    .unwrap();
                let _ = send_stream.send_trailers(trailers.headers().clone());
                return Err(e);
            }
        }

        // Send final empty STAT packet to indicate end of stats (as done in fsutil send.go line 182)
        let final_stat_packet = Packet {
            r#type: PacketType::PacketStat as i32,
            stat: None,
            id: 0,
            data: vec![],
        };
        Self::send_grpc_packet(&mut send_stream, &final_stat_packet).await?;

        tracing::info!("Sent all STAT packets (including final empty STAT), now waiting for REQ packets from BuildKit");

        // Now listen for REQ packets from BuildKit and send the requested files
        // We need to accumulate data across multiple chunks to form complete gRPC messages
        let mut buffer = Vec::new();
        let mut received_fin = false;

        loop {
            // Read next chunk from request stream
            match request_stream.data().await {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    let _ = request_stream.flow_control().release_capacity(chunk.len());

                    // Try to parse complete gRPC messages from buffer
                    while buffer.len() >= 5 {
                        // Read gRPC frame header (5 bytes)
                        let compressed = buffer[0];
                        let length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;

                        if buffer.len() < 5 + length {
                            // Not enough data for complete message yet
                            break;
                        }

                        // Extract the complete message
                        let message_data = buffer[5..5+length].to_vec();
                        buffer.drain(0..5+length);

                        if compressed != 0 {
                            tracing::warn!("Received compressed message, skipping");
                            continue;
                        }

                        // Decode the packet
                        let packet = match Packet::decode(Bytes::from(message_data)) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("Failed to decode packet: {}", e);
                                continue;
                            }
                        };

                        let packet_type = PacketType::try_from(packet.r#type).unwrap_or(PacketType::PacketStat);
                        tracing::debug!("Received packet type: {:?}, id: {}, has_stat: {}",
                            packet_type, packet.id, packet.stat.is_some());

                        match packet_type {
                            PacketType::PacketReq => {
                                // BuildKit is requesting file data for a specific ID
                                tracing::info!("Received REQ packet with id: {}", packet.id);

                                if let Some(file_path) = file_map.get(&packet.id) {
                                    tracing::info!("Sending file data for id {}: {}", packet.id, file_path.display());
                                    if let Err(e) = Self::send_file_data_packets(file_path.clone(), packet.id, &mut send_stream).await {
                                        tracing::error!("Failed to send file data: {}", e);
                                    }
                                } else {
                                    tracing::warn!("File ID {} not found in map (probably a directory, ignoring)", packet.id);
                                }
                            }
                            PacketType::PacketFin => {
                                // BuildKit is signaling it's done requesting files
                                tracing::info!("Received FIN packet from BuildKit, ending transfer");
                                received_fin = true;
                                break;
                            }
                            _ => {
                                tracing::debug!("Ignoring packet type: {:?}", packet_type);
                            }
                        }
                    }

                    // Check if we received FIN and should exit the outer loop
                    if received_fin {
                        break;
                    }
                }
                Some(Err(e)) => {
                    tracing::error!("Error reading request stream: {}", e);
                    break;
                }
                None => {
                    tracing::info!("Request stream ended");
                    break;
                }
            }
        }

        tracing::info!("DiffCopy completed, sending FIN packet");

        // Send FIN packet to indicate all transfers are complete
        let fin_packet = Packet {
            r#type: PacketType::PacketFin as i32,
            stat: None,
            id: 0,
            data: vec![],
        };

        Self::send_grpc_packet(&mut send_stream, &fin_packet).await?;
        tracing::debug!("Sent final FIN packet");

        // Send success trailers
        let trailers = Response::builder()
            .header("grpc-status", "0")
            .body(())
            .unwrap();

        send_stream.send_trailers(trailers.headers().clone())
            .map_err(|e| Error::Http2Stream { source: e })?;

        Ok(())
    }

    /// Send STAT packets using depth-first traversal
    /// This is the correct way to send files to BuildKit's fsutil validator
    /// which requires files in depth-first order with entries sorted alphabetically within each directory
    ///
    /// If followpaths is Some, only sends files in the list and their parent directories
    fn send_stat_packets_dfs<'a>(
        path: std::path::PathBuf,
        prefix: String,
        stream: &'a mut h2::SendStream<Bytes>,
        file_map: &'a mut std::collections::HashMap<u32, std::path::PathBuf>,
        id_counter: &'a mut u32,
        followpaths: Option<&'a Vec<String>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            use crate::proto::fsutil::types::{Packet, packet::PacketType, Stat};

            tracing::debug!("send_stat_packets_dfs: {} (prefix: {}, followpaths: {:?})", path.display(), prefix, followpaths);

            // Build set of paths to include if followpaths is specified
            let include_paths = if let Some(paths) = followpaths {
                let mut set = std::collections::HashSet::new();
                for p in paths {
                    set.insert(p.clone());
                    // Add all parent directories
                    let mut parent = p.as_str();
                    while let Some(idx) = parent.rfind('/') {
                        parent = &parent[..idx];
                        set.insert(parent.to_string());
                    }
                }
                tracing::debug!("Built include_paths set with {} entries: {:?}", set.len(), set);
                Some(set)
            } else {
                None
            };

            // Read all entries in this directory
            let mut entries = Vec::new();
            let mut dir_entries = tokio::fs::read_dir(&path).await?;

            while let Some(entry) = dir_entries.next_entry().await? {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy().to_string();
                let entry_path = entry.path();
                let metadata = entry.metadata().await?;

                entries.push((name, entry_path, metadata));
            }

            // Sort entries alphabetically by name (fsutil requirement)
            entries.sort_by(|a, b| a.0.cmp(&b.0));

            // Process entries in sorted order (depth-first)
            for (name, entry_path, metadata) in entries {
                let rel_path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", prefix, name)
                };

                // Skip if not in include_paths (when filtering is enabled)
                if let Some(ref paths) = include_paths {
                    if !paths.contains(&rel_path) {
                        tracing::debug!("Skipping {} (not in followpaths)", rel_path);
                        eprintln!("DFS: Skipping {} (not in include_paths)", rel_path);
                        continue;
                    } else {
                        eprintln!("DFS: Including {} (found in include_paths)", rel_path);
                    }
                }

                let entry_id = *id_counter;
                *id_counter += 1;

                // Create and send STAT packet for this entry
                let mut stat = Stat {
                    path: rel_path.clone(),
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    // For directories, size must be 0 (fsutil protocol requirement)
                    size: if metadata.is_dir() { 0 } else { metadata.len() as i64 },
                    mod_time: 0,
                    linkname: String::new(),
                    devmajor: 0,
                    devminor: 0,
                    xattrs: std::collections::HashMap::new(),
                };

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let unix_mode = metadata.permissions().mode();
                    stat.mode = GoFileMode::from(UnixMode::from(unix_mode)).as_u32();
                }

                #[cfg(not(unix))]
                {
                    // On non-Unix platforms, construct mode in Go FileMode format directly
                    stat.mode = if metadata.is_dir() {
                        0x80000000 | 0o755  // GO_MODE_DIR | 0o755
                    } else {
                        0o644  // Just permissions for regular files
                    };
                }

                let mode = stat.mode;
                let size = stat.size;
                let path_sent = stat.path.clone();
                let stat_packet = Packet {
                    r#type: PacketType::PacketStat as i32,
                    stat: Some(stat),
                    id: entry_id,
                    data: vec![],
                };

                tracing::info!("Sending STAT packet for: {} (id: {}, mode: 0o{:o})", path_sent, entry_id, mode);
                eprintln!("DFS: Sending STAT #{}: {} ({}, mode: 0o{:o} / 0x{:x}, size: {}, is_dir: {})",
                         entry_id, path_sent,
                         if metadata.is_dir() { "DIR" } else { "FILE" },
                         mode, mode, size, (mode & 0o040000) != 0);
                Self::send_grpc_packet(stream, &stat_packet).await?;

                // Store file path in map for later data requests (only for files)
                if metadata.is_file() {
                    file_map.insert(entry_id, entry_path.clone());
                }

                // Recursively process directories
                if metadata.is_dir() {
                    Self::send_stat_packets_dfs(entry_path, rel_path, stream, file_map, id_counter, followpaths).await?;
                }
            }

            Ok(())
        })
    }

    /// Recursively collect all entries (files and directories) from a path
    /// Returns a vector of (relative_path, absolute_path, metadata) tuples
    fn collect_entries_recursive<'a>(
        path: std::path::PathBuf,
        prefix: String,
        result: &'a mut Vec<(String, std::path::PathBuf, std::fs::Metadata)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            tracing::debug!("Collecting entries from: {} (prefix: {})", path.display(), prefix);

            let mut entries = tokio::fs::read_dir(&path).await?;

            while let Some(entry) = entries.next_entry().await? {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                let rel_path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", prefix, name)
                };

                let entry_path = entry.path();
                let metadata = entry.metadata().await?;

                // Add this entry to result
                result.push((rel_path.clone(), entry_path.clone(), metadata.clone()));

                // Recursively handle directories
                if metadata.is_dir() {
                    Self::collect_entries_recursive(entry_path, rel_path, result).await?;
                }
            }

            Ok(())
        })
    }

    /// Send file data as DATA packets in response to a REQ
    async fn send_file_data_packets(
        path: std::path::PathBuf,
        req_id: u32,
        stream: &mut h2::SendStream<Bytes>,
    ) -> Result<()> {
        use crate::proto::fsutil::types::{Packet, packet::PacketType};
        use tokio::io::AsyncReadExt;

        tracing::info!("Sending file data for: {} (id: {})", path.display(), req_id);

        let mut file = tokio::fs::File::open(&path).await
            ?;

        let mut buffer = vec![0u8; 32 * 1024]; // 32KB chunks

        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }

            let data_packet = Packet {
                r#type: PacketType::PacketData as i32,
                stat: None,
                id: req_id,
                data: buffer[..n].to_vec(),
            };

            Self::send_grpc_packet(stream, &data_packet).await?;
        }

        // Send empty DATA packet to indicate end of this file
        // (NOT a FIN packet - FIN is sent only at the very end of all transfers)
        let eof_packet = Packet {
            r#type: PacketType::PacketData as i32,
            stat: None,
            id: req_id,
            data: vec![],
        };

        Self::send_grpc_packet(stream, &eof_packet).await?;
        tracing::debug!("Sent EOF (empty DATA) packet for id: {}", req_id);

        Ok(())
    }

    /// Send a single gRPC-framed packet
    async fn send_grpc_packet(
        stream: &mut h2::SendStream<Bytes>,
        packet: &crate::proto::fsutil::types::Packet,
    ) -> Result<()> {
        use prost::Message as ProstMessage;
        use crate::proto::fsutil::types::packet::PacketType;

        let mut payload = Vec::new();
        packet.encode(&mut payload)?;

        // Add gRPC framing (5-byte prefix)
        let mut framed = Vec::new();
        framed.push(0); // No compression
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);

        let packet_type = PacketType::try_from(packet.r#type).ok();
        tracing::trace!("Sending packet: type={:?}, id={}, data_len={}, total_frame_len={}",
            packet_type, packet.id, packet.data.len(), framed.len());

        stream.send_data(Bytes::from(framed), false)
            .map_err(|e| Error::Http2Stream { source: e })?;

        // Give the h2 stream a chance to flush
        tokio::task::yield_now().await;

        Ok(())
    }

    /// Handle Auth.GetTokenAuthority request
    #[allow(dead_code)]
    async fn handle_auth_get_token_authority(&self, payload: Bytes) -> Result<Bytes> {
        use crate::proto::moby::filesync::v1::{GetTokenAuthorityRequest, GetTokenAuthorityResponse};

        let request = GetTokenAuthorityRequest::decode(payload)
            .map_err(|e| Error::decode("GetTokenAuthorityRequest", e))?;

        tracing::info!("Auth.GetTokenAuthority request for host: {}", request.host);

        // Return empty response - we don't implement token-based auth
        // BuildKit will detect empty public_key and fall back to Credentials method
        let response = GetTokenAuthorityResponse {
            public_key: vec![],
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Handle Auth.Credentials request
    async fn handle_auth_credentials(&self, payload: Bytes) -> Result<Bytes> {
        use crate::proto::moby::filesync::v1::CredentialsRequest;
        use tonic::Request;
        use crate::proto::moby::filesync::v1::auth_server::Auth;

        let request = CredentialsRequest::decode(payload)
            .map_err(|e| Error::decode("CredentialsRequest", e))?;

        tracing::info!("Auth.Credentials request for host: {}", request.host);

        // Use AuthServer if configured, otherwise return empty credentials
        let response = if let Some(auth) = &self.auth {
            match auth.credentials(Request::new(request.clone())).await {
                Ok(resp) => {
                    let inner = resp.into_inner();
                    if !inner.username.is_empty() {
                        tracing::debug!("Returning credentials for host: {} (username: {})",
                            request.host, inner.username);
                    } else {
                        tracing::debug!("No credentials found for host: {}, returning empty", request.host);
                    }
                    inner
                }
                Err(status) => {
                    tracing::warn!("Failed to get credentials: {}, returning empty", status.message());
                    use crate::proto::moby::filesync::v1::CredentialsResponse;
                    CredentialsResponse {
                        username: String::new(),
                        secret: String::new(),
                    }
                }
            }
        } else {
            tracing::debug!("No auth configured, returning empty credentials");
            use crate::proto::moby::filesync::v1::CredentialsResponse;
            CredentialsResponse {
                username: String::new(),
                secret: String::new(),
            }
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Handle Auth.FetchToken request
    async fn handle_auth_fetch_token(&self, _payload: Bytes) -> Result<Bytes> {
        use crate::proto::moby::filesync::v1::FetchTokenResponse;

        tracing::info!("Auth.FetchToken called");

        let response = FetchTokenResponse {
            token: String::new(),
            expires_in: 0,
            issued_at: 0,
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Handle Secrets.GetSecret request
    async fn handle_secrets_get_secret(&self, payload: Bytes) -> Result<Bytes> {
        use crate::proto::moby::secrets::v1::GetSecretRequest;

        let request = GetSecretRequest::decode(payload)
            .map_err(|e| Error::decode("GetSecretRequest", e))?;

        tracing::info!("Secrets.GetSecret request for ID: {}", request.id);

        // If secrets service is not configured, return empty data
        let response = if let Some(secrets) = &self.secrets {
            // Use the SecretsServer's get_secret implementation through the Secrets trait
            use tonic::Request;
            use crate::proto::moby::secrets::v1::secrets_server::Secrets;

            match secrets.get_secret(Request::new(request.clone())).await {
                Ok(resp) => {
                    let inner = resp.into_inner();
                    tracing::debug!("Returning secret '{}' ({} bytes)", request.id, inner.data.len());
                    inner
                }
                Err(status) => {
                    tracing::warn!("Secret '{}' not found: {}", request.id, status.message());
                    return Err(Error::SecretNotFound(status.message().to_string()));
                }
            }
        } else {
            tracing::warn!("Secrets service not configured");
            return Err(Error::SecretsNotConfigured);
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Handle Health.Check request
    async fn handle_health_check(&self, _payload: Bytes) -> Result<Bytes> {
        tracing::info!("Health check called");

        // Health check response: status = SERVING (1)
        // The proto definition is:
        // message HealthCheckResponse {
        //   enum ServingStatus {
        //     UNKNOWN = 0;
        //     SERVING = 1;
        //     NOT_SERVING = 2;
        //   }
        //   ServingStatus status = 1;
        // }

        // Manually encode: field 1, varint type, value 1
        let response = vec![0x08, 0x01]; // field 1 (0x08 = 0001|000) = value 1
        Ok(Bytes::from(response))
    }
}

/// A stream that wraps BytesMessage channels to implement AsyncRead + AsyncWrite
struct MessageStream {
    inbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BytesMessage>>>,
    outbound_tx: mpsc::Sender<BytesMessage>,
    read_buffer: Vec<u8>,
    read_pos: usize,
}

impl MessageStream {
    fn new(
        inbound_rx: mpsc::Receiver<BytesMessage>,
        outbound_tx: mpsc::Sender<BytesMessage>,
    ) -> Self {
        Self {
            inbound_rx: Arc::new(tokio::sync::Mutex::new(inbound_rx)),
            outbound_tx,
            read_buffer: Vec::new(),
            read_pos: 0,
        }
    }
}

impl AsyncRead for MessageStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // If we have buffered data, return it
        if self.read_pos < self.read_buffer.len() {
            let remaining = &self.read_buffer[self.read_pos..];
            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            self.read_pos += to_copy;

            // Clear buffer if fully consumed
            if self.read_pos >= self.read_buffer.len() {
                self.read_buffer.clear();
                self.read_pos = 0;
            }

            return Poll::Ready(Ok(()));
        }

        // Try to receive next message
        let inbound_rx = self.inbound_rx.clone();
        let mut rx = match inbound_rx.try_lock() {
            Ok(rx) => rx,
            Err(_) => return Poll::Pending,
        };

        match rx.poll_recv(cx) {
            Poll::Ready(Some(msg)) => {
                self.read_buffer = msg.data;
                self.read_pos = 0;

                let to_copy = self.read_buffer.len().min(buf.remaining());
                buf.put_slice(&self.read_buffer[..to_copy]);
                self.read_pos = to_copy;

                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())), // EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MessageStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let msg = BytesMessage {
            data: buf.to_vec(),
        };

        // Try to send immediately (non-blocking)
        match self.outbound_tx.try_send(msg) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Channel is full, would block
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Channel closed",
                )))
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
