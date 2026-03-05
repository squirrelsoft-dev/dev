//! File synchronization protocol implementation for BuildKit sessions

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::proto::fsutil::types::{Packet, packet::PacketType, Stat};
use crate::proto::moby::filesync::v1::{
    file_sync_server::FileSync,
};

/// File sync server implementation
///
/// Implements the BuildKit file synchronization protocol for streaming
/// local build context files to BuildKit.
#[derive(Debug, Clone)]
pub struct FileSyncServer {
    root_path: PathBuf,
}

impl FileSyncServer {
    /// Create a new file sync server
    ///
    /// # Arguments
    ///
    /// * `root_path` - Root directory to serve files from
    ///
    /// # Example
    ///
    /// ```
    /// use buildkit_client::session::FileSyncServer;
    /// use std::path::PathBuf;
    ///
    /// let sync = FileSyncServer::new(PathBuf::from("."));
    /// ```
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: root_path.into(),
        }
    }

    /// Get the root path
    pub fn get_root_path(&self) -> PathBuf {
        self.root_path.clone()
    }

    /// Check if a path is within the allowed root directory
    fn validate_path(&self, rel_path: &str) -> Result<PathBuf> {
        let full_path = self.root_path.join(rel_path);
        let canonical = std::fs::canonicalize(&full_path)?;

        if !canonical.starts_with(&self.root_path) {
            return Err(Error::PathOutsideRoot {
                path: rel_path.to_string(),
            });
        }

        Ok(canonical)
    }

    /// Create a stat packet from file metadata
    async fn create_stat_packet(path: &Path, rel_path: &str) -> Result<Packet> {
        let metadata = fs::metadata(path).await?;

        let mut stat = Stat {
            path: rel_path.to_string(),
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
            stat.mode = metadata.permissions().mode();
        }

        if metadata.is_dir() {
            stat.mode |= 0o040000; // S_IFDIR
        } else if metadata.is_file() {
            stat.mode |= 0o100000; // S_IFREG
        }

        Ok(Packet {
            r#type: PacketType::PacketStat as i32,
            stat: Some(stat),
            id: 0,
            data: vec![],
        })
    }

    /// Read directory and send stat packets
    fn read_directory<'a>(
        path: &'a Path,
        prefix: &'a str,
        tx: &'a tokio::sync::mpsc::Sender<std::result::Result<Packet, Status>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut entries = fs::read_dir(path).await?;

            while let Some(entry) = entries.next_entry().await? {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                let rel_path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", prefix, name)
                };

                let entry_path = entry.path();
                let stat_packet = Self::create_stat_packet(&entry_path, &rel_path).await?;

                tx.send(Ok(stat_packet)).await
                    .map_err(|_| Error::send_failed("STAT packet", "channel closed"))?;

                // Recursively handle directories
                if entry_path.is_dir() {
                    FileSyncServer::read_directory(&entry_path, &rel_path, tx).await?;
                }
            }

            Ok(())
        })
    }

    /// Send file data in chunks
    async fn send_file_data(
        &self,
        path: &Path,
        id: u32,
        tx: &tokio::sync::mpsc::Sender<std::result::Result<Packet, Status>>,
    ) -> Result<()> {
        let mut file = fs::File::open(path).await?;

        let mut buffer = vec![0u8; 1024 * 1024]; // 1MB chunks

        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }

            let packet = Packet {
                r#type: PacketType::PacketData as i32,
                stat: None,
                id,
                data: buffer[..n].to_vec(),
            };

            tx.send(Ok(packet)).await
                .map_err(|_| Error::send_failed("DATA packet", "channel closed"))?;
        }

        // Send FIN packet
        let fin_packet = Packet {
            r#type: PacketType::PacketFin as i32,
            stat: None,
            id,
            data: vec![],
        };

        tx.send(Ok(fin_packet)).await
            .map_err(|_| Error::send_failed("FIN packet", "channel closed"))?;

        Ok(())
    }
}

#[tonic::async_trait]
impl FileSync for FileSyncServer {
    type DiffCopyStream = ReceiverStream<std::result::Result<Packet, Status>>;
    type TarStreamStream = ReceiverStream<std::result::Result<Packet, Status>>;

    async fn diff_copy(
        &self,
        request: Request<tonic::Streaming<Packet>>,
    ) -> std::result::Result<Response<Self::DiffCopyStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let server = self.clone();

        tokio::spawn(async move {
            tracing::debug!("Starting DiffCopy session");

            // First, send all file stats
            if let Err(e) = FileSyncServer::read_directory(&server.root_path, "", &tx).await {
                tracing::error!("Failed to read directory: {}", e);
                let _ = tx.send(Err(Status::internal(format!("Failed to read directory: {}", e)))).await;
                return;
            }

            // Process incoming requests
            while let Ok(Some(packet)) = in_stream.message().await {
                let packet_type = PacketType::try_from(packet.r#type).unwrap_or(PacketType::PacketStat);

                match packet_type {
                    PacketType::PacketReq => {
                        // Client is requesting file data
                        if let Some(ref stat) = packet.stat {
                            let path = match server.validate_path(&stat.path) {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::error!("Invalid path {}: {}", stat.path, e);
                                    continue;
                                }
                            };

                            if path.is_file() {
                                if let Err(e) = server.send_file_data(&path, packet.id, &tx).await {
                                    tracing::error!("Failed to send file data: {}", e);
                                    let _ = tx.send(Err(Status::internal(format!("Failed to send file: {}", e)))).await;
                                    return;
                                }
                            }
                        }
                    }
                    _ => {
                        tracing::debug!("Received packet type: {:?}", packet_type);
                    }
                }
            }

            tracing::debug!("DiffCopy session completed");
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn tar_stream(
        &self,
        request: Request<tonic::Streaming<Packet>>,
    ) -> std::result::Result<Response<Self::TarStreamStream>, Status> {
        // TarStream is similar to DiffCopy but uses tar format
        // For simplicity, we'll use the same implementation
        self.diff_copy(request).await
    }
}
