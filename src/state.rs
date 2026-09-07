use std::sync::Arc;

use crate::config::AppConfig;
use crate::models::video::VideoBackend;
use crate::services::cloudfront::CloudfrontService;
use crate::services::downloader::DownloaderService;
use crate::services::ffmpeg::FfmpegService;
use crate::services::ffprobe::FfprobeService;
use crate::services::mediaconvert::MediaConvertService;
use crate::services::s3::S3Service;

/// All shared application state, passed to route handlers via `web::Data<AppState>`.
///
/// A concrete struct, not `web::Data<dyn Trait>`: the `Arc<dyn VideoBackend>` field
/// is the only dynamic dispatch, and keeping the rest concrete avoids the `!Sized`
/// guard that `web::Data` extraction otherwise needs.
pub struct AppState {
    pub config: AppConfig,
    pub backend: Arc<dyn VideoBackend>,
    pub s3: S3Service,
    pub cf: CloudfrontService,
    pub probe: FfprobeService,
    pub ffmpeg: FfmpegService,
    pub mediaconvert: MediaConvertService,
    pub downloader: DownloaderService,
}
