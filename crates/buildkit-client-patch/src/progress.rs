//! Build progress monitoring and reporting

use crate::error::Result;
use crate::proto::moby::buildkit::v1::StatusResponse;

/// Trait for handling build progress updates
pub trait ProgressHandler: Send {
    /// Called when the build starts
    fn on_start(&mut self) -> Result<()>;

    /// Called for each status update
    fn on_status(&mut self, status: StatusResponse) -> Result<()>;

    /// Called when the build completes successfully
    fn on_complete(&mut self) -> Result<()>;

    /// Called when an error occurs
    fn on_error(&mut self, error: &str) -> Result<()>;
}

/// Console progress handler that prints to stdout
pub struct ConsoleProgressHandler {
    verbose: bool,
}

impl ConsoleProgressHandler {
    /// Create a new console progress handler
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }
}

impl ProgressHandler for ConsoleProgressHandler {
    fn on_start(&mut self) -> Result<()> {
        println!("🚀 Build started...");
        Ok(())
    }

    fn on_status(&mut self, status: StatusResponse) -> Result<()> {
        for vertex in status.vertexes {
            if vertex.completed.is_some() {
                println!("✅ {}", vertex.name);
            } else if vertex.started.is_some() {
                println!("⏳ {}...", vertex.name);
            }

            if self.verbose && !vertex.cached {
                // Show non-cached operations
                tracing::debug!("Vertex: {} ({})", vertex.name, vertex.digest);
            }
        }

        // Show logs
        for log in status.logs {
            if self.verbose {
                if let Ok(msg) = String::from_utf8(log.msg) {
                    print!("{}", msg);
                }
            }
        }

        Ok(())
    }

    fn on_complete(&mut self) -> Result<()> {
        println!("✨ Build completed successfully!");
        Ok(())
    }

    fn on_error(&mut self, error: &str) -> Result<()> {
        eprintln!("❌ Build failed: {}", error);
        Ok(())
    }
}

/// JSON progress handler that outputs structured JSON
#[derive(Default)]
pub struct JsonProgressHandler;

impl JsonProgressHandler {
    pub fn new() -> Self {
        Self
    }
}

impl ProgressHandler for JsonProgressHandler {
    fn on_start(&mut self) -> Result<()> {
        println!("{{\"status\": \"started\"}}");
        Ok(())
    }

    fn on_status(&mut self, status: StatusResponse) -> Result<()> {
        let json = serde_json::json!({
            "vertexes": status.vertexes.iter().map(|v| {
                serde_json::json!({
                    "digest": v.digest,
                    "name": v.name,
                    "cached": v.cached,
                    "started": v.started.as_ref().map(|t| t.seconds),
                    "completed": v.completed.as_ref().map(|t| t.seconds),
                    "error": v.error,
                })
            }).collect::<Vec<_>>(),
            "statuses": status.statuses.iter().map(|s| {
                serde_json::json!({
                    "vertex": s.vertex,
                    "current": s.current,
                    "total": s.total,
                    "timestamp": s.timestamp.as_ref().map(|t| t.seconds),
                })
            }).collect::<Vec<_>>(),
        });

        match serde_json::to_string(&json) {
            Ok(s) => println!("{}", s),
            Err(e) => tracing::error!("Failed to serialize progress JSON: {}", e),
        }
        Ok(())
    }

    fn on_complete(&mut self) -> Result<()> {
        println!("{{\"status\": \"completed\"}}");
        Ok(())
    }

    fn on_error(&mut self, error: &str) -> Result<()> {
        let json = serde_json::json!({
            "status": "failed",
            "error": error,
        });
        match serde_json::to_string(&json) {
            Ok(s) => println!("{}", s),
            Err(e) => tracing::error!("Failed to serialize error JSON: {}", e),
        }
        Ok(())
    }
}

/// Silent progress handler that doesn't output anything
#[derive(Default)]
pub struct SilentProgressHandler;

impl SilentProgressHandler {
    pub fn new() -> Self {
        Self
    }
}

impl ProgressHandler for SilentProgressHandler {
    fn on_start(&mut self) -> Result<()> {
        Ok(())
    }

    fn on_status(&mut self, _status: StatusResponse) -> Result<()> {
        Ok(())
    }

    fn on_complete(&mut self) -> Result<()> {
        Ok(())
    }

    fn on_error(&mut self, _error: &str) -> Result<()> {
        Ok(())
    }
}
