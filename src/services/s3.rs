use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::S3Config;

/// Above this, the upload is split into parts rather than read into memory.
///
/// The image service reads the whole file and calls `PutObject`, which is fine when
/// the file is a photo. A video is routinely two orders of magnitude larger, and a
/// couple of concurrent uploads doing that would take the node's memory with them —
/// on a spot fleet, where the container's limit is the whole box's, that is an OOM
/// that kills live requests as well as the upload.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Part size for the multipart path. S3's minimum is 5 MiB for every part but the
/// last; 16 MiB keeps the part count sane on a large file (10,000 parts is the
/// ceiling, so this covers 156 GiB) without holding much at once.
const PART_SIZE: usize = 16 * 1024 * 1024;

pub struct S3Service {
    client: Client,
    bucket: String,
}

impl S3Service {
    pub fn new(cfg: &S3Config) -> Self {
        let credentials = Credentials::new(
            &cfg.access_key_id,
            &cfg.secret_access_key,
            None,
            None,
            "config",
        );
        let mut builder = aws_sdk_s3::Config::builder()
            .credentials_provider(credentials)
            .region(Region::new(cfg.region.clone()))
            .force_path_style(cfg.force_path_style())
            .behavior_version_latest();

        // Point at MinIO (or any S3-compatible server) when configured; otherwise the
        // SDK resolves the real AWS endpoint for the region.
        let endpoint = cfg.endpoint_url.trim();
        if !endpoint.is_empty() {
            info!("S3 endpoint override: {}", endpoint);
            builder = builder.endpoint_url(endpoint);
        }

        let client = Client::from_conf(builder.build());
        Self {
            client,
            bucket: cfg.bucket_name.clone(),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// `s3://bucket/key`, the form MediaConvert wants for inputs and destinations.
    pub fn uri(&self, key: &str) -> String {
        format!("s3://{}/{}", self.bucket, key)
    }

    /// Upload a local file, choosing single-shot or multipart by size.
    pub async fn save(&self, from: &Path, to: &str, mime_type: &str) -> Result<()> {
        let len = tokio::fs::metadata(from)
            .await
            .with_context(|| format!("Failed to stat {:?} for S3 upload", from))?
            .len();

        if len > MULTIPART_THRESHOLD {
            info!("S3 multipart upload: {:?} ({} bytes) → {}", from, len, to);
            return self.save_multipart(from, to, mime_type).await;
        }

        // Read into memory first — ByteStream::from_path with behavior_version_latest()
        // can deadlock when the SDK tries to compute a streaming checksum on a file
        // body. Bounded by MULTIPART_THRESHOLD, so this cannot run away.
        let bytes = tokio::fs::read(from)
            .await
            .with_context(|| format!("Failed to read {:?} for S3 upload", from))?;
        info!("S3 upload: {:?} ({} bytes) → {}", from, bytes.len(), to);
        let body = ByteStream::from(bytes);
        timeout(
            Duration::from_secs(300),
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(to)
                .body(body)
                .content_type(mime_type)
                .send(),
        )
        .await
        .with_context(|| format!("S3 PutObject timed out after 300s for key={}", to))?
        .with_context(|| format!("S3 PutObject failed for key={}", to))?;
        Ok(())
    }

    /// Multipart upload, one part at a time.
    ///
    /// Sequential on purpose, for the reason the image service transcodes its sizes
    /// sequentially: concurrent uploads through this SDK's connection pool have a
    /// history of wedging, and the bottleneck here is the network rather than the
    /// number of requests in flight.
    async fn save_multipart(&self, from: &Path, to: &str, mime_type: &str) -> Result<()> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(to)
            .content_type(mime_type)
            .send()
            .await
            .with_context(|| format!("S3 CreateMultipartUpload failed for key={}", to))?;

        let upload_id = created
            .upload_id()
            .context("S3 CreateMultipartUpload returned no upload id")?
            .to_string();

        match self.upload_parts(from, to, &upload_id).await {
            Ok(parts) => {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(to)
                    .upload_id(&upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await
                    .with_context(|| format!("S3 CompleteMultipartUpload failed for key={}", to))?;
                info!("S3 multipart upload done: {}", to);
                Ok(())
            }
            Err(e) => {
                // An abandoned multipart upload is billed storage that nothing lists
                // and nobody notices, so tidy up before surfacing the real error.
                if let Err(abort_err) = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(to)
                    .upload_id(&upload_id)
                    .send()
                    .await
                {
                    tracing::warn!("S3 AbortMultipartUpload failed for key={to}: {abort_err}");
                }
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        from: &Path,
        to: &str,
        upload_id: &str,
    ) -> Result<Vec<CompletedPart>> {
        let mut file = tokio::fs::File::open(from)
            .await
            .with_context(|| format!("Failed to open {:?} for multipart upload", from))?;

        let mut parts: Vec<CompletedPart> = Vec::new();
        let mut part_number = 1i32;

        loop {
            let mut buf = vec![0u8; PART_SIZE];
            let mut filled = 0usize;
            // read() is free to return short of the buffer; a short part that is not
            // the last one is rejected by S3, so fill deliberately rather than
            // trusting one call.
            while filled < PART_SIZE {
                let n = file
                    .read(&mut buf[filled..])
                    .await
                    .with_context(|| format!("Failed reading {:?} for part {}", from, part_number))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            buf.truncate(filled);

            debug!("S3 part {} ({} bytes) → {}", part_number, filled, to);
            let uploaded = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(to)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(buf))
                .send()
                .await
                .with_context(|| format!("S3 UploadPart {} failed for key={}", part_number, to))?;

            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(uploaded.e_tag().map(str::to_string))
                    .build(),
            );
            part_number += 1;
        }

        Ok(parts)
    }

    /// Does this key exist? Used to confirm a MediaConvert job's outputs really
    /// landed before they are written into the record as playable.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_not_found() {
                    Ok(false)
                } else {
                    Err(anyhow::Error::new(svc))
                        .with_context(|| format!("S3 HeadObject failed for key={}", key))
                }
            }
        }
    }
}
