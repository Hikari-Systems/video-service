//! AWS Elemental MediaConvert — the transcoder.
//!
//! The shape difference from the image service lives here. ImageMagick is a
//! subprocess: you call it, it returns, the file exists. A MediaConvert job is a
//! remote thing that is *accepted* in milliseconds and *completes* minutes later,
//! long after the request that asked for it has gone. Nothing in this module waits
//! for a job; it submits them and reads their state, and
//! [`crate::services::sweeper`] is what turns those two halves back into a finished
//! record.
//!
//! One consequence worth stating plainly, because it drives the job splitting:
//! **a job's outputs are released together.** There is no way to have the small
//! rendition appear early within a job that also produces the large one. Getting a
//! proxy in front of a viewer quickly therefore means submitting it as its own job
//! at a higher queue priority — see [`crate::config::TranscodeConfig::proxy_rendition`].

use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_mediaconvert::config::Region;
use aws_sdk_mediaconvert::types::{
    AacCodingMode, AacSettings, AccelerationMode, AccelerationSettings, AudioCodec,
    AudioCodecSettings, AudioDefaultSelection, AudioDescription, AudioSelector, ContainerSettings,
    ContainerType, FileGroupSettings, H264RateControlMode, H264Settings, HlsGroupSettings,
    HlsSegmentControl, Input, InputTimecodeSource, JobSettings, Mp4MoovPlacement, Mp4Settings,
    Output, OutputGroup, OutputGroupSettings, OutputGroupType, StatusUpdateInterval,
    VideoCodec, VideoCodecSettings, VideoDescription, VideoSelector,
};
use aws_sdk_mediaconvert::Client;
use tracing::{debug, info};
use uuid::Uuid;

use crate::config::{MediaConvertConfig, RenditionConfig, S3Config};

/// A job we have handed to MediaConvert.
#[derive(Debug, Clone)]
pub struct SubmittedJob {
    pub id: String,
    pub renditions: Vec<String>,
}

/// MediaConvert's current view of a job.
#[derive(Debug, Clone)]
pub struct JobState {
    pub id: String,
    /// `SUBMITTED` | `PROGRESSING` | `COMPLETE` | `ERROR` | `CANCELED`.
    pub status: String,
    pub percent_complete: Option<i32>,
    pub error: Option<String>,
}

/// What to transcode, and into what.
pub struct SubmitRequest<'a> {
    pub video_id: Uuid,
    pub category: &'a str,
    /// `s3://bucket/key` of the source.
    pub input_uri: String,
    /// `s3://bucket/` for the FILE_GROUP. MediaConvert appends the input's basename
    /// and each output's name modifier, which is what makes the resulting keys
    /// predictable enough to store without listing the bucket afterwards.
    pub file_destination: String,
    /// `s3://bucket/<category>-<id>-` for HLS groups, whose destination is used
    /// literally as the prefix its manifest and segments share.
    pub hls_destination_prefix: String,
    pub renditions: Vec<(String, RenditionConfig)>,
    /// Submit at the proxy priority rather than the default one.
    pub expedited: bool,
}

pub struct MediaConvertService {
    /// `None` when no role is configured. The service still starts and still serves
    /// already-transcoded videos; only submission is unavailable. A box with no
    /// transcoding credentials should be able to run the read path.
    client: Option<Client>,
    role_arn: String,
    queue_arn: String,
    priority: Option<i32>,
    proxy_priority: Option<i32>,
    accelerated: bool,
    status_interval_seconds: u32,
}

impl MediaConvertService {
    pub fn new(cfg: &MediaConvertConfig, s3: &S3Config) -> Self {
        if !cfg.is_configured() {
            info!("MediaConvert: no roleArn configured — job submission is disabled");
            return Self {
                client: None,
                role_arn: String::new(),
                queue_arn: String::new(),
                priority: None,
                proxy_priority: None,
                accelerated: false,
                status_interval_seconds: cfg.status_update_interval_seconds,
            };
        }

        // Same credentials as S3 by default — the service reads and writes one
        // bucket, and MediaConvert reaches it through its own role rather than
        // through these.
        let credentials = Credentials::new(
            &s3.access_key_id,
            &s3.secret_access_key,
            None,
            None,
            "config",
        );
        let region = cfg.region_or(&s3.region);
        let mut builder = aws_sdk_mediaconvert::Config::builder()
            .credentials_provider(credentials)
            .region(Region::new(region.clone()))
            .behavior_version_latest();

        // MediaConvert used to require an account-specific endpoint discovered via
        // DescribeEndpoints. The regional endpoint works directly now, so this is
        // only for pointing at a local stand-in.
        let endpoint = cfg.endpoint_url.trim();
        if !endpoint.is_empty() {
            info!("MediaConvert endpoint override: {}", endpoint);
            builder = builder.endpoint_url(endpoint);
        }

        info!("MediaConvert: region {} role {}", region, cfg.role_arn);
        Self {
            client: Some(Client::from_conf(builder.build())),
            role_arn: cfg.role_arn.clone(),
            queue_arn: cfg.queue_arn.clone(),
            priority: cfg.priority,
            proxy_priority: cfg.proxy_priority,
            accelerated: cfg.is_accelerated(),
            status_interval_seconds: cfg.status_update_interval_seconds,
        }
    }

    pub fn is_available(&self) -> bool {
        self.client.is_some()
    }

    /// Submit one job and return its id. Does not wait for it.
    pub async fn submit(&self, req: SubmitRequest<'_>) -> Result<SubmittedJob> {
        let client = self
            .client
            .as_ref()
            .context("MediaConvert is not configured (mediaconvert.roleArn is empty)")?;

        if req.renditions.is_empty() {
            anyhow::bail!("refusing to submit a MediaConvert job with no outputs");
        }

        let names: Vec<String> = req.renditions.iter().map(|(n, _)| n.clone()).collect();
        let groups = build_output_groups(
            &req.renditions,
            &req.file_destination,
            &req.hls_destination_prefix,
        );
        if groups.is_empty() {
            anyhow::bail!(
                "no usable MediaConvert outputs for renditions {:?} — check their width/height/ladder",
                names
            );
        }

        let settings = JobSettings::builder()
            .inputs(
                Input::builder()
                    .file_input(&req.input_uri)
                    .audio_selectors(
                        "Audio Selector 1",
                        AudioSelector::builder()
                            .default_selection(AudioDefaultSelection::Default)
                            .build(),
                    )
                    .video_selector(VideoSelector::builder().build())
                    .timecode_source(InputTimecodeSource::Zerobased)
                    .build(),
            )
            .set_output_groups(Some(groups))
            .build();

        let priority = if req.expedited {
            self.proxy_priority.or(self.priority)
        } else {
            self.priority
        };

        let mut call = client
            .create_job()
            .role(&self.role_arn)
            .settings(settings)
            // Carried back on every EventBridge job-state-change event, so a
            // completion can be matched to a row without parsing output filenames.
            .user_metadata("videoId", req.video_id.to_string())
            .user_metadata("category", req.category.to_string())
            .user_metadata("renditions", names.join(","))
            .status_update_interval(status_update_interval(self.status_interval_seconds));

        if !self.queue_arn.trim().is_empty() {
            call = call.queue(&self.queue_arn);
        }
        if let Some(p) = priority {
            call = call.priority(p);
        }
        if self.accelerated {
            // PREFERRED rather than ENABLED: acceleration does not support every
            // input, and ENABLED makes an unsupported one a hard job failure.
            // PREFERRED silently runs it unaccelerated instead, which is the right
            // trade for a queue fed by whatever people upload.
            call = call.acceleration_settings(
                AccelerationSettings::builder()
                    .mode(AccelerationMode::Preferred)
                    .build(),
            );
        }

        let out = call
            .send()
            .await
            .with_context(|| format!("MediaConvert CreateJob failed for video {}", req.video_id))?;

        let id = out
            .job()
            .and_then(|j| j.id())
            .context("MediaConvert CreateJob returned no job id")?
            .to_string();

        info!(
            "MediaConvert job {} submitted for video {} ({:?}, expedited={})",
            id, req.video_id, names, req.expedited
        );
        Ok(SubmittedJob { id, renditions: names })
    }

    /// Poll one job.
    ///
    /// Polling rather than only listening for EventBridge events because the sweep
    /// has to be able to recover a record whose event was missed — a node that was
    /// down when the event fired would otherwise leave the row PROGRESSING forever.
    /// Events are the fast path; this is the one that guarantees eventual truth.
    pub async fn job_state(&self, job_id: &str) -> Result<JobState> {
        let client = self
            .client
            .as_ref()
            .context("MediaConvert is not configured (mediaconvert.roleArn is empty)")?;

        let out = client
            .get_job()
            .id(job_id)
            .send()
            .await
            .with_context(|| format!("MediaConvert GetJob failed for job {}", job_id))?;

        let job = out.job().context("MediaConvert GetJob returned no job")?;
        let status = job
            .status()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "SUBMITTED".to_string());

        debug!("MediaConvert job {job_id}: {status}");
        Ok(JobState {
            id: job_id.to_string(),
            status,
            percent_complete: job.job_percent_complete(),
            error: job.error_message().map(str::to_string),
        })
    }
}

/// MediaConvert accepts a fixed set of update intervals, so an arbitrary number of
/// seconds has to be rounded to one of them rather than passed through.
fn status_update_interval(seconds: u32) -> StatusUpdateInterval {
    match seconds {
        0..=10 => StatusUpdateInterval::Seconds10,
        11..=12 => StatusUpdateInterval::Seconds12,
        13..=15 => StatusUpdateInterval::Seconds15,
        16..=20 => StatusUpdateInterval::Seconds20,
        21..=30 => StatusUpdateInterval::Seconds30,
        31..=60 => StatusUpdateInterval::Seconds60,
        61..=120 => StatusUpdateInterval::Seconds120,
        121..=180 => StatusUpdateInterval::Seconds180,
        181..=240 => StatusUpdateInterval::Seconds240,
        241..=300 => StatusUpdateInterval::Seconds300,
        301..=360 => StatusUpdateInterval::Seconds360,
        361..=420 => StatusUpdateInterval::Seconds420,
        421..=480 => StatusUpdateInterval::Seconds480,
        481..=540 => StatusUpdateInterval::Seconds540,
        _ => StatusUpdateInterval::Seconds600,
    }
}

/// Split the requested renditions into MediaConvert output groups.
///
/// Progressive renditions share one FILE_GROUP, because they are independent MP4s
/// that differ only in size and can be named apart with a modifier. Each HLS
/// rendition needs its **own** HLS_GROUP: a group emits one master manifest over its
/// own ladder, so two HLS renditions in one group would collide on that manifest.
fn build_output_groups(
    renditions: &[(String, RenditionConfig)],
    file_destination: &str,
    hls_destination_prefix: &str,
) -> Vec<OutputGroup> {
    let mut groups: Vec<OutputGroup> = Vec::new();

    let progressive: Vec<Output> = renditions
        .iter()
        .filter(|(_, cfg)| !cfg.is_hls())
        .filter_map(|(name, cfg)| progressive_output(name, cfg))
        .collect();

    if !progressive.is_empty() {
        groups.push(
            OutputGroup::builder()
                .name("File Group")
                .output_group_settings(
                    OutputGroupSettings::builder()
                        .r#type(OutputGroupType::FileGroupSettings)
                        .file_group_settings(
                            FileGroupSettings::builder()
                                .destination(file_destination)
                                .build(),
                        )
                        .build(),
                )
                .set_outputs(Some(progressive))
                .build(),
        );
    }

    for (name, cfg) in renditions.iter().filter(|(_, c)| c.is_hls()) {
        let rungs = cfg.rungs();
        if rungs.is_empty() {
            continue;
        }
        let outputs: Vec<Output> = rungs
            .iter()
            .map(|rung| {
                hls_output(
                    &format!("_{}x{}", rung.width, rung.height),
                    rung.width,
                    rung.height,
                    rung.bitrate,
                )
            })
            .collect();

        // The HLS destination is a path prefix, not a directory: MediaConvert writes
        // `<prefix>.m3u8` as the master manifest and the variants beside it. That is
        // exactly the shape `helpers::playback::hls_prefix` reverses when it works
        // out what a signed cookie has to cover.
        groups.push(
            OutputGroup::builder()
                .name(format!("HLS {name}"))
                .output_group_settings(
                    OutputGroupSettings::builder()
                        .r#type(OutputGroupType::HlsGroupSettings)
                        .hls_group_settings(
                            HlsGroupSettings::builder()
                                .destination(format!("{hls_destination_prefix}{name}"))
                                .segment_length(cfg.segment_seconds.unwrap_or(6))
                                .min_segment_length(0)
                                .segment_control(HlsSegmentControl::SegmentedFiles)
                                .build(),
                        )
                        .build(),
                )
                .set_outputs(Some(outputs))
                .build(),
        );
    }

    groups
}

/// One progressive MP4 output.
///
/// Returns `None` when the rendition has no dimensions, rather than submitting an
/// output that would inherit the source's size — a "small" rendition silently
/// produced at 4K is worse than a rejected job.
fn progressive_output(name: &str, cfg: &RenditionConfig) -> Option<Output> {
    let (width, height) = (cfg.width?, cfg.height?);
    let bitrate = cfg.bitrate.unwrap_or(2_500_000);

    Some(
        Output::builder()
            // Produces `<basename>-<name><ext>`, which is the same
            // `category-id-variant` convention the image service uses for its sizes.
            .name_modifier(format!("-{name}"))
            .container_settings(
                ContainerSettings::builder()
                    .container(ContainerType::Mp4)
                    .mp4_settings(
                        Mp4Settings::builder()
                            // The output has to be faststart for exactly the reason
                            // the source so often is not. Producing a rendition with
                            // the index at the tail would reintroduce, in our own
                            // output, the bug this service exists to work around.
                            .moov_placement(Mp4MoovPlacement::ProgressiveDownload)
                            .build(),
                    )
                    .build(),
            )
            .video_description(video_description(width, height, bitrate))
            .audio_descriptions(audio_description())
            .build(),
    )
}

fn hls_output(name_modifier: &str, width: i32, height: i32, bitrate: i32) -> Output {
    Output::builder()
        .name_modifier(name_modifier)
        .container_settings(
            ContainerSettings::builder()
                .container(ContainerType::M3U8)
                .build(),
        )
        .video_description(video_description(width, height, bitrate))
        .audio_descriptions(audio_description())
        .build()
}

fn video_description(width: i32, height: i32, bitrate: i32) -> VideoDescription {
    VideoDescription::builder()
        .width(width)
        .height(height)
        .codec_settings(
            VideoCodecSettings::builder()
                .codec(VideoCodec::H264)
                .h264_settings(
                    H264Settings::builder()
                        // QVBR treats the bitrate as a ceiling rather than a target,
                        // so a static talking head costs a fraction of it and a
                        // fast-moving clip is capped by it. Billing is per output
                        // minute either way; the saving is in delivery bandwidth and
                        // in not wasting bits on frames that do not need them.
                        .rate_control_mode(H264RateControlMode::Qvbr)
                        .max_bitrate(bitrate)
                        .build(),
                )
                .build(),
        )
        .build()
}

fn audio_description() -> AudioDescription {
    AudioDescription::builder()
        .audio_source_name("Audio Selector 1")
        .codec_settings(
            AudioCodecSettings::builder()
                .codec(AudioCodec::Aac)
                .aac_settings(
                    AacSettings::builder()
                        .bitrate(128_000)
                        .coding_mode(AacCodingMode::CodingMode20)
                        .sample_rate(48_000)
                        .build(),
                )
                .build(),
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progressive(width: i32, height: i32) -> RenditionConfig {
        RenditionConfig {
            width: Some(width),
            height: Some(height),
            bitrate: Some(1_000_000),
            kind: "progressive".into(),
            ..Default::default()
        }
    }

    fn hls(ladder: &str) -> RenditionConfig {
        RenditionConfig {
            kind: "hls".into(),
            ladder: ladder.into(),
            ..Default::default()
        }
    }

    /// Independent MP4s belong in one group, told apart by name modifier — that is
    /// what makes the output keys predictable enough to store without listing S3.
    #[test]
    fn progressive_renditions_share_one_file_group() {
        let groups = build_output_groups(
            &[
                ("proxy".into(), progressive(640, 360)),
                ("hd".into(), progressive(1920, 1080)),
            ],
            "s3://bucket/",
            "s3://bucket/clip-1-",
        );
        assert_eq!(groups.len(), 1);
        let outputs = groups[0].outputs();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name_modifier(), Some("-proxy"));
        assert_eq!(outputs[1].name_modifier(), Some("-hd"));
    }

    /// Each HLS rendition emits its own master manifest, so sharing a group would
    /// have the second one overwrite the first.
    #[test]
    fn each_hls_rendition_gets_its_own_group() {
        let groups = build_output_groups(
            &[
                ("hls".into(), hls("640x360@800000,1280x720@2500000")),
                ("hlsAlt".into(), hls("1920x1080@5000000")),
            ],
            "s3://bucket/",
            "s3://bucket/clip-1-",
        );
        assert_eq!(groups.len(), 2);
        // Two rungs in the first ladder, one in the second.
        assert_eq!(groups[0].outputs().len(), 2);
        assert_eq!(groups[1].outputs().len(), 1);
    }

    /// The HLS destination is the prefix the manifest and segments share, and it has
    /// to match what the playback helper reverses to build a cookie policy.
    #[test]
    fn the_hls_destination_is_the_shared_prefix() {
        let groups = build_output_groups(
            &[("hls".into(), hls("640x360@800000"))],
            "s3://bucket/",
            "s3://bucket/clip-1-",
        );
        let dest = groups[0]
            .output_group_settings()
            .unwrap()
            .hls_group_settings()
            .unwrap()
            .destination()
            .unwrap();
        assert_eq!(dest, "s3://bucket/clip-1-hls");
    }

    /// A rendition with no dimensions would come out at the source's size, so a
    /// "proxy" could silently be 4K. Dropping it is the safer failure.
    #[test]
    fn a_rendition_without_dimensions_produces_no_output() {
        let groups = build_output_groups(
            &[("broken".into(), RenditionConfig { kind: "progressive".into(), ..Default::default() })],
            "s3://bucket/",
            "s3://bucket/clip-1-",
        );
        assert!(groups.is_empty());
    }

    /// Our own outputs must not reproduce the tail-index bug the service exists to
    /// work around.
    #[test]
    fn progressive_outputs_are_faststart() {
        let groups = build_output_groups(
            &[("proxy".into(), progressive(640, 360))],
            "s3://bucket/",
            "s3://bucket/clip-1-",
        );
        let placement = groups[0].outputs()[0]
            .container_settings()
            .unwrap()
            .mp4_settings()
            .unwrap()
            .moov_placement()
            .unwrap();
        assert_eq!(placement, &Mp4MoovPlacement::ProgressiveDownload);
    }

    #[test]
    fn the_status_interval_snaps_to_an_accepted_value() {
        assert_eq!(status_update_interval(60), StatusUpdateInterval::Seconds60);
        assert_eq!(status_update_interval(45), StatusUpdateInterval::Seconds60);
        assert_eq!(status_update_interval(1), StatusUpdateInterval::Seconds10);
        assert_eq!(status_update_interval(99_999), StatusUpdateInterval::Seconds600);
    }
}
