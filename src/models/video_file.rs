use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::fs;
use tracing::{error, warn};
use uuid::Uuid;

use super::video::{VideoBackend, VideoRecord};

/// JSON-on-disk metadata, for local runs and single-node setups.
///
/// Note what it does **not** implement: neither claim method is overridden, so the
/// trait's defaults return an empty vec and the sweep does nothing on this backend.
/// That is deliberate. Without an exclusive claim, every replica would submit its
/// own MediaConvert job for the same video — and unlike a duplicated ImageMagick
/// run, that is a duplicated bill. `main.rs` says so at startup rather than letting
/// it be discovered on an invoice.
pub struct FileBackend {
    parent_path: String,
}

impl FileBackend {
    pub fn new(parent_path: String) -> Self {
        Self { parent_path }
    }

    fn path_for(&self, id: &str) -> PathBuf {
        PathBuf::from(&self.parent_path).join(format!("{}.json", id))
    }
}

#[async_trait]
impl VideoBackend for FileBackend {
    async fn get(&self, id: &str) -> Result<Option<VideoRecord>> {
        let path = self.path_for(id);
        match fs::read_to_string(&path).await {
            Ok(json) => {
                let record: VideoRecord = serde_json::from_str(&json)
                    .with_context(|| format!("Failed to parse video JSON for id={}", id))?;
                Ok(Some(record))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                error!("Error loading video details for id={}: {}", id, e);
                Err(e).with_context(|| format!("Failed to read video file for id={}", id))
            }
        }
    }

    async fn upsert(&self, video: VideoRecord) -> Result<VideoRecord> {
        let record = VideoRecord {
            id: Some(video.id.unwrap_or_else(Uuid::new_v4)),
            ..video
        };
        let id_str = record.id.unwrap().to_string();
        let path = self.path_for(&id_str);
        let json = serde_json::to_string(&record)
            .with_context(|| format!("Failed to serialise video id={}", id_str))?;
        fs::write(&path, json)
            .await
            .with_context(|| format!("Failed to write video file for id={}", id_str))
            .map_err(|e| {
                warn!("Error saving video id={}: {}", id_str, e);
                e
            })?;
        Ok(record)
    }
}
