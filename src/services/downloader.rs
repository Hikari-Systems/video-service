use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::debug;

pub struct DownloaderService {
    client: Client,
}

pub struct DownloadedFile {
    pub local_path: PathBuf,
    pub mime_type: String,
    pub bytes: u64,
}

impl DownloaderService {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("video-service/0.1")
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Download `url` to `dest`, streaming.
    ///
    /// Streamed rather than buffered — unlike the image service, which reads the
    /// whole response before writing it. A source video is routinely large enough
    /// that holding it in memory would be the difference between a working node and
    /// an OOM that takes live requests with it.
    pub async fn download(&self, url: &str, dest: &Path) -> Result<DownloadedFile> {
        debug!("Downloading {} → {:?}", url, dest);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("HTTP GET failed for {}", url))?;

        if !response.status().is_success() {
            anyhow::bail!("Error downloading video: {} status={}", url, response.status());
        }

        let mime_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let mut file = File::create(dest)
            .await
            .with_context(|| format!("Failed to create {:?}", dest))?;

        let mut written: u64 = 0;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("Failed reading body for {}", url))?;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("Failed to write to {:?}", dest))?;
            written += chunk.len() as u64;
        }
        file.flush().await?;

        debug!("Downloaded {} ({} bytes, {})", url, written, mime_type);
        Ok(DownloadedFile {
            local_path: dest.to_path_buf(),
            mime_type,
            bytes: written,
        })
    }
}

impl Default for DownloaderService {
    fn default() -> Self {
        Self::new()
    }
}
