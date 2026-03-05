use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::DevError;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

#[derive(Serialize, Deserialize, Default)]
pub struct CacheMetadata {
    pub entries: HashMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub etag: Option<String>,
    pub timestamp: u64,
}

pub struct CacheManager {
    base_dir: PathBuf,
    metadata_path: PathBuf,
}

impl CacheManager {
    pub fn new() -> Result<Self, DevError> {
        let base = dirs::cache_dir()
            .ok_or_else(|| DevError::Cache("cannot determine cache directory".into()))?;
        let base_dir = base.join("dev").join("collections");
        std::fs::create_dir_all(&base_dir)?;
        let metadata_path = base_dir.join("metadata.json");
        Ok(Self {
            base_dir,
            metadata_path,
        })
    }

    fn load_metadata(&self) -> CacheMetadata {
        std::fs::read_to_string(&self.metadata_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_metadata(&self, meta: &CacheMetadata) -> Result<(), DevError> {
        let json = serde_json::to_string_pretty(meta)?;
        std::fs::write(&self.metadata_path, json)?;
        Ok(())
    }

    /// Check if a cached entry is still valid (within TTL).
    pub fn is_fresh(&self, key: &str) -> bool {
        let meta = self.load_metadata();
        if let Some(entry) = meta.entries.get(key) {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now.saturating_sub(entry.timestamp) < CACHE_TTL.as_secs()
        } else {
            false
        }
    }

    /// Get the ETag for a cached key.
    pub fn etag(&self, key: &str) -> Option<String> {
        let meta = self.load_metadata();
        meta.entries.get(key).and_then(|e| e.etag.clone())
    }

    /// Read cached data for a key.
    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.base_dir.join(key);
        std::fs::read(&path).ok()
    }

    /// Write data to cache, updating the ETag and timestamp.
    pub fn write(&self, key: &str, data: &[u8], etag: Option<String>) -> Result<(), DevError> {
        let path = self.base_dir.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;

        let mut meta = self.load_metadata();
        meta.entries.insert(
            key.to_string(),
            CacheEntry {
                etag,
                timestamp: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
        );
        self.save_metadata(&meta)?;
        Ok(())
    }
}
