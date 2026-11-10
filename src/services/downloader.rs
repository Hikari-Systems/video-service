use anyhow::{Context, Result};
use futures_util::StreamExt;
use hs_utils::http_client::SendTraced as _;
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
    ///
    /// The source URL is the caller's and is often presigned, i.e. a credential:
    /// the span records its origin only, reqwest's errors are stripped of it, and
    /// every message here names the origin, never the path or query.
    pub async fn download(&self, url: &str, dest: &Path) -> Result<DownloadedFile> {
        let origin = url_origin(url);
        debug!("Downloading from {} → {:?}", origin, dest);
        let response = self
            .client
            .get(url)
            .send_traced_opaque()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("HTTP GET failed for {}", origin))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Error downloading video from {}: status={}",
                origin,
                response.status()
            );
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
            let chunk = chunk
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("Failed reading body from {}", origin))?;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("Failed to write to {:?}", dest))?;
            written += chunk.len() as u64;
        }
        file.flush().await?;

        debug!(
            "Downloaded from {} ({} bytes, {})",
            origin, written, mime_type
        );
        Ok(DownloadedFile {
            local_path: dest.to_path_buf(),
            mime_type,
            bytes: written,
        })
    }
}

/// `scheme://host[:port]` of `url`, for logs: a source URL's path and query can be
/// a credential (a presigned URL). Something unparseable is not echoed at all.
pub fn url_origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if u.has_host() => u.origin().ascii_serialization(),
        _ => "<unparseable url>".to_string(),
    }
}

impl Default for DownloaderService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::url_origin;

    #[test]
    fn a_presigned_source_url_is_logged_by_origin_only() {
        assert_eq!(
            url_origin(
                "https://bucket.s3.amazonaws.com/a/b.mp4?X-Amz-Signature=abc&X-Amz-Credential=k"
            ),
            "https://bucket.s3.amazonaws.com"
        );
        assert_eq!(
            url_origin("http://user:pw@example.com:8080/x"),
            "http://example.com:8080"
        );
        assert_eq!(url_origin("not a url"), "<unparseable url>");
    }
}
