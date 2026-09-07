use anyhow::{Context, Result};
use hs_utils::config::{
    apply_env_overrides, deep_merge, deser_i64_or_str, deser_opt_bool_or_str, deser_opt_i32_or_str,
    deser_u16_or_str, deser_u32_or_str, prepare_config,
};
pub use hs_utils::db::DbConfig;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

// ── Structs mirror config.json exactly; every key is camelCase ───────────────

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_port", deserialize_with = "deser_u16_or_str")]
    pub port: u16,
}

fn default_port() -> u16 { 3000 }

#[derive(Debug, Deserialize, Clone)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String { "info".to_string() }

#[derive(Debug, Deserialize, Clone)]
pub struct VideoMetadataConfig {
    #[serde(rename = "parentPath", default = "default_metadata_path")]
    pub parent_path: String,
    #[serde(default = "default_storage")]
    pub storage: String,
}

fn default_metadata_path() -> String { "/metadata".to_string() }
fn default_storage() -> String { "file".to_string() }

/// The local ffmpeg/ffprobe pair.
///
/// Neither is used to *transcode* — that is MediaConvert's job. They exist for the
/// two cheap, bounded operations that have to happen inside the upload request:
/// probing the source, and the stream-copy remux that makes an already-compatible
/// upload progressively playable. See [`crate::services::ffmpeg`].
#[derive(Debug, Deserialize, Clone)]
pub struct FfmpegConfig {
    #[serde(default = "default_ffmpeg_bin")]
    pub bin: String,
    #[serde(rename = "probeBin", default = "default_ffprobe_bin")]
    pub probe_bin: String,
    /// Ceiling on either subprocess. A remux is a byte copy, so a file that has not
    /// finished in minutes is a file something is wrong with — and it is holding a
    /// request open while it does nothing.
    #[serde(
        rename = "timeoutSeconds",
        default = "default_ffmpeg_timeout",
        deserialize_with = "deser_u32_or_str"
    )]
    pub timeout_seconds: u32,
}

fn default_ffmpeg_bin() -> String { "/usr/bin/ffmpeg".to_string() }
fn default_ffprobe_bin() -> String { "/usr/bin/ffprobe".to_string() }
fn default_ffmpeg_timeout() -> u32 { 300 }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct S3Config {
    #[serde(rename = "bucketName", default)]
    pub bucket_name: String,
    #[serde(rename = "accessKeyId", default)]
    pub access_key_id: String,
    #[serde(rename = "secretAccessKey", default)]
    pub secret_access_key: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// Override the S3 endpoint — set this to point at MinIO or another S3-compatible
    /// server for local testing. Empty means the real AWS endpoint for the region.
    #[serde(rename = "endpointUrl", default)]
    pub endpoint_url: String,
    #[serde(
        rename = "forcePathStyle",
        default,
        deserialize_with = "deser_opt_bool_or_str"
    )]
    pub force_path_style: Option<bool>,
}

impl S3Config {
    pub fn force_path_style(&self) -> bool {
        self.force_path_style
            .unwrap_or(!self.endpoint_url.trim().is_empty())
    }
}

fn default_region() -> String { "us-east-1".to_string() }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct CloudfrontConfig {
    #[serde(default)]
    pub url: String,
    #[serde(
        rename = "expirySeconds",
        default = "default_expiry",
        deserialize_with = "deser_i64_or_str"
    )]
    pub expiry_seconds: i64,
    #[serde(rename = "keypairId", default)]
    pub keypair_id: String,
    #[serde(rename = "privateKey", default)]
    pub private_key: String,
    #[serde(rename = "privateKeyFile", default)]
    pub private_key_file: String,
    /// Domain for the signed playback cookies. Empty leaves the cookie host-only,
    /// which is correct whenever the player is served from the CDN domain itself.
    #[serde(rename = "cookieDomain", default)]
    pub cookie_domain: String,
}

fn default_expiry() -> i64 { 10100 }

/// AWS Elemental MediaConvert — the transcoder.
///
/// `roleArn` is not optional in practice: MediaConvert assumes that role to read the
/// input and write the outputs, so a job submitted without one fails at AWS rather
/// than here. Left empty by default all the same, so the service starts (and serves
/// already-transcoded videos) on a box that has no transcoding credentials at all.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct MediaConvertConfig {
    #[serde(rename = "roleArn", default)]
    pub role_arn: String,
    #[serde(rename = "queueArn", default)]
    pub queue_arn: String,
    /// Defaults to `s3.region` when empty — the common case is one region for both.
    #[serde(default)]
    pub region: String,
    #[serde(rename = "endpointUrl", default)]
    pub endpoint_url: String,
    /// Queue priority for the main rendition job (-50..=50, higher runs sooner).
    #[serde(default, deserialize_with = "deser_opt_i32_or_str")]
    pub priority: Option<i32>,
    /// Priority for the proxy job, which exists to finish first. Higher than
    /// `priority` or the split buys nothing.
    #[serde(rename = "proxyPriority", default, deserialize_with = "deser_opt_i32_or_str")]
    pub proxy_priority: Option<i32>,
    /// Accelerated transcoding. Costs more per minute and is worth it on long
    /// inputs, where it is the difference between a wait and a very long wait.
    #[serde(default, deserialize_with = "deser_opt_bool_or_str")]
    pub accelerated: Option<bool>,
    #[serde(
        rename = "statusUpdateIntervalSeconds",
        default = "default_status_interval",
        deserialize_with = "deser_u32_or_str"
    )]
    pub status_update_interval_seconds: u32,
}

fn default_status_interval() -> u32 { 60 }

impl MediaConvertConfig {
    pub fn is_configured(&self) -> bool {
        !self.role_arn.trim().is_empty()
    }

    pub fn region_or(&self, fallback: &str) -> String {
        let r = self.region.trim();
        if r.is_empty() { fallback.to_string() } else { r.to_string() }
    }

    pub fn is_accelerated(&self) -> bool {
        self.accelerated.unwrap_or(false)
    }
}

/// One output rendition.
///
/// `kind` decides the shape of what lands in S3, and therefore how it is delivered:
/// `progressive` is a single MP4 addressable by one signed URL, `hls` is a manifest
/// plus a directory of segments that needs signed *cookies*. See
/// [`crate::services::cloudfront`].
#[derive(Debug, Deserialize, Clone, Default)]
pub struct RenditionConfig {
    #[serde(deserialize_with = "deser_opt_i32_or_str", default)]
    pub width: Option<i32>,
    #[serde(deserialize_with = "deser_opt_i32_or_str", default)]
    pub height: Option<i32>,
    /// Target bitrate in bits per second. QVBR treats it as a ceiling, not a target,
    /// so a simple clip costs less than this and a complex one is capped by it.
    #[serde(deserialize_with = "deser_opt_i32_or_str", default)]
    pub bitrate: Option<i32>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub extension: Option<String>,
    /// `hls` only: the variant ladder, `WxH@bitrate` entries separated by commas.
    #[serde(default)]
    pub ladder: String,
    #[serde(
        rename = "segmentSeconds",
        default,
        deserialize_with = "deser_opt_i32_or_str"
    )]
    pub segment_seconds: Option<i32>,
}

fn default_kind() -> String { "progressive".to_string() }

/// One rung of an HLS ladder, parsed from `WxH@bitrate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderRung {
    pub width: i32,
    pub height: i32,
    pub bitrate: i32,
}

impl RenditionConfig {
    pub fn is_hls(&self) -> bool {
        self.kind.eq_ignore_ascii_case("hls")
    }

    pub fn ext(&self) -> String {
        self.extension
            .clone()
            .unwrap_or_else(|| if self.is_hls() { ".m3u8".into() } else { ".mp4".into() })
    }

    pub fn mime(&self) -> String {
        self.mime_type.clone().unwrap_or_else(|| {
            if self.is_hls() {
                "application/vnd.apple.mpegurl".into()
            } else {
                "video/mp4".into()
            }
        })
    }

    /// The ladder for an HLS rendition, falling back to a single rung built from
    /// `width`/`height`/`bitrate` so an `hls` entry with no `ladder` still works.
    pub fn rungs(&self) -> Vec<LadderRung> {
        let parsed: Vec<LadderRung> = self
            .ladder
            .split(',')
            .filter_map(|entry| parse_rung(entry.trim()))
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
        match (self.width, self.height) {
            (Some(w), Some(h)) => vec![LadderRung {
                width: w,
                height: h,
                bitrate: self.bitrate.unwrap_or(2_500_000),
            }],
            _ => Vec::new(),
        }
    }
}

/// `1280x720@2500000` → a rung. Anything else is `None` and is skipped, so one
/// malformed entry cannot take the whole ladder down with it.
fn parse_rung(entry: &str) -> Option<LadderRung> {
    if entry.is_empty() {
        return None;
    }
    let (dims, bitrate) = entry.split_once('@')?;
    let (w, h) = dims.split_once(['x', 'X'])?;
    Some(LadderRung {
        width: w.trim().parse().ok()?,
        height: h.trim().parse().ok()?,
        bitrate: bitrate.trim().parse().ok()?,
    })
}

/// How the `original` derivative is labelled in S3.
///
/// Only the **copy** path keeps the source's own extension and type; the remux path
/// always produces a faststart MP4, because that is the only thing
/// `-movflags +faststart` can produce. So these two settings name the object, they
/// do not choose a format — they exist for buckets with extension-driven lifecycle
/// or content-type policies, not as a format switch. Changing `extension` to
/// something that is not an MP4 family suffix will label MP4 bytes wrongly.
#[derive(Debug, Deserialize, Clone)]
pub struct OriginalConfig {
    #[serde(rename = "mimeType", default = "default_original_mime")]
    pub mime_type: String,
    #[serde(default = "default_original_extension")]
    pub extension: String,
}

impl Default for OriginalConfig {
    fn default() -> Self {
        Self {
            mime_type: default_original_mime(),
            extension: default_original_extension(),
        }
    }
}

fn default_original_mime() -> String { "video/mp4".to_string() }
fn default_original_extension() -> String { ".mp4".to_string() }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TranscodeConfig {
    #[serde(default = "default_processing")]
    pub processing: String,
    #[serde(rename = "renditionKeys", default)]
    pub rendition_keys: String,
    /// The order the playback fallback walks when the exact rendition asked for is
    /// not ready. Cheapest first is the useful order: a viewer waiting on `hd` would
    /// rather watch `proxy` now than nothing. Empty means "config order".
    #[serde(rename = "fallbackOrder", default)]
    pub fallback_order: String,
    /// The rendition submitted as its own job, ahead of the rest.
    ///
    /// MediaConvert releases a job's outputs all at once, so "give me the small one
    /// first" is a second job rather than an early output. Naming one here splits
    /// the submission in two: a cheap rendition at `proxyPriority`, everything else
    /// behind it. Empty disables the split and submits one job for everything.
    #[serde(rename = "proxyRendition", default)]
    pub proxy_rendition: String,
    /// Settings for the `original` derivative — the playable copy of the upload.
    /// A **named** field, so serde consumes it before the flattened `renditions`
    /// map: `original` is a reserved word and can never be a rendition key.
    #[serde(default)]
    pub original: OriginalConfig,
    /// The background pass. Named for the same reason as `original`, and reserved
    /// in the same way.
    #[serde(rename = "transcodeSweep", default)]
    pub transcode_sweep: TranscodeSweepConfig,
    #[serde(flatten)]
    pub renditions: HashMap<String, RenditionConfig>,
    #[serde(rename = "renditionSets", default)]
    pub rendition_sets: HashMap<String, String>,
}

/// The background pass that submits and reconciles MediaConvert jobs.
///
/// It carries more weight here than it does for images. An image transcode is a
/// subprocess that either returns or does not; a MediaConvert job is a remote thing
/// that completes minutes after the request that asked for it has gone. Without
/// this pass, nothing ever writes the renditions back.
#[derive(Debug, Deserialize, Clone)]
pub struct TranscodeSweepConfig {
    #[serde(default, deserialize_with = "deser_opt_bool_or_str")]
    pub enabled: Option<bool>,
    #[serde(
        rename = "intervalSeconds",
        default = "default_sweep_interval",
        deserialize_with = "deser_u32_or_str"
    )]
    pub interval_seconds: u32,
    /// Videos whose jobs are *submitted* per pass. Small: each one is an S3 read and
    /// a MediaConvert API call, and there is no hurry — the queue does the waiting.
    #[serde(
        rename = "batchSize",
        default = "default_sweep_batch",
        deserialize_with = "deser_u32_or_str"
    )]
    pub batch_size: u32,
    /// Videos whose in-flight jobs are *polled* per pass. Larger than `batchSize`
    /// because a poll is one cheap API call and a completed job that nobody has
    /// noticed is a rendition nobody can play.
    #[serde(
        rename = "reconcileBatchSize",
        default = "default_sweep_reconcile",
        deserialize_with = "deser_u32_or_str"
    )]
    pub reconcile_batch_size: u32,
    /// How long a **submit** claim is held before another node may retry the video.
    ///
    /// A **lease, not a flag**: a node that dies mid-pass frees its work when the
    /// clock passes, with no operator and no stuck rows. Long, because what it
    /// guards is expensive — a second node submitting the same video is a second
    /// MediaConvert bill — and because the window it covers includes an S3 upload of
    /// arbitrary size.
    #[serde(
        rename = "leaseSeconds",
        default = "default_sweep_lease",
        deserialize_with = "deser_u32_or_str"
    )]
    pub lease_seconds: u32,
    /// How long a **reconcile** claim is held — that is, how long before an
    /// in-flight job is polled again.
    ///
    /// Deliberately separate from, and far shorter than, `leaseSeconds`. Reusing the
    /// submit lease here looks harmless and is not: it makes the poll interval equal
    /// to the duplicate-submission guard, so a job that finished seconds after being
    /// polled goes unnoticed for the whole of that guard. Measured against real
    /// MediaConvert, a transcode that completed in about 4 seconds sat unrecorded
    /// for 15 minutes — the video was playable in S3 and `pending` in the API.
    ///
    /// Still a lease rather than nothing, so a fleet does not have every replica
    /// polling every running job on every pass.
    #[serde(
        rename = "pollSeconds",
        default = "default_sweep_poll",
        deserialize_with = "deser_u32_or_str"
    )]
    pub poll_seconds: u32,
}

impl Default for TranscodeSweepConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            interval_seconds: default_sweep_interval(),
            batch_size: default_sweep_batch(),
            reconcile_batch_size: default_sweep_reconcile(),
            lease_seconds: default_sweep_lease(),
            poll_seconds: default_sweep_poll(),
        }
    }
}

impl TranscodeSweepConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

fn default_processing() -> String { "deferred".to_string() }
fn default_sweep_interval() -> u32 { 60 }
fn default_sweep_batch() -> u32 { 2 }
fn default_sweep_reconcile() -> u32 { 10 }
fn default_sweep_lease() -> u32 { 900 }
fn default_sweep_poll() -> u32 { 30 }

impl TranscodeConfig {
    /// The rendition keys configured for a category, falling back to the global list.
    ///
    /// The lookup is **case-insensitive**, and has to be. Categories are lower-cased
    /// everywhere else in the pipeline — on upload, in the S3 key, in the sweep's
    /// claim query — so a `renditionSets` entry written as `shortClip` would
    /// otherwise never match anything, silently fall back to the global list, and
    /// leave the sweep judging that category against the wrong rendition count.
    /// Config keys are case-preserving by design (the whole reason this service does
    /// not use the `config` crate), so normalising has to happen here.
    pub fn rendition_keys_for_category(&self, category: &str) -> Vec<String> {
        let keys_str = if category.is_empty() {
            Some(self.rendition_keys.clone())
        } else {
            let wanted = category.to_lowercase();
            self.rendition_sets
                .iter()
                .find(|(k, _)| k.to_lowercase() == wanted)
                .map(|(_, v)| v.clone())
        };
        split_list(&keys_str.unwrap_or_else(|| self.rendition_keys.clone()))
    }

    pub fn get_rendition(&self, key: &str) -> Option<&RenditionConfig> {
        self.renditions.get(key)
    }

    /// The order [`crate::helpers::playback`] tries renditions in once the exact one
    /// asked for is missing: `fallbackOrder` first (for the entries that are actually
    /// configured for this category), then anything left over in category order, so a
    /// rendition omitted from `fallbackOrder` is still reachable rather than invisible.
    pub fn fallback_order_for_category(&self, category: &str) -> Vec<String> {
        let configured = self.rendition_keys_for_category(category);
        let mut out: Vec<String> = split_list(&self.fallback_order)
            .into_iter()
            .filter(|k| configured.iter().any(|c| c == k))
            .collect();
        for key in configured {
            if !out.contains(&key) {
                out.push(key);
            }
        }
        out
    }
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(rename = "videoMetadata", default)]
    pub video_metadata: VideoMetadataConfig,
    #[serde(default)]
    pub ffmpeg: FfmpegConfig,
    #[serde(rename = "uploadDir", default)]
    pub upload_dir: String,
    #[serde(default)]
    pub s3: S3Config,
    #[serde(default)]
    pub cloudfront: CloudfrontConfig,
    #[serde(default)]
    pub mediaconvert: MediaConvertConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub transcode: TranscodeConfig,
}

impl Default for ServerConfig {
    fn default() -> Self { Self { port: default_port() } }
}
impl Default for LogConfig {
    fn default() -> Self { Self { level: default_log_level() } }
}
impl Default for VideoMetadataConfig {
    fn default() -> Self {
        Self { parent_path: default_metadata_path(), storage: default_storage() }
    }
}
impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
            probe_bin: default_ffprobe_bin(),
            timeout_seconds: default_ffmpeg_timeout(),
        }
    }
}

impl AppConfig {
    /// Load configuration in priority order (lowest → highest):
    ///
    /// 1. `config.json` in the working directory
    /// 2. `/sandbox/config.json` — deep-merged on top; silently ignored if absent
    /// 3. Env vars with `__` separator, e.g. `s3__bucketName=my-bucket`
    pub fn load() -> Result<Self> {
        let mut root: Value = match std::fs::read_to_string("config.json") {
            Ok(s) => serde_json::from_str(&s).context("Failed to parse config.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Value::Object(Default::default())
            }
            Err(e) => return Err(e).context("Failed to read config.json"),
        };

        match std::fs::read_to_string("/sandbox/config.json") {
            Ok(s) => {
                let overlay: Value = serde_json::from_str(&s)
                    .context("Failed to parse /sandbox/config.json")?;
                deep_merge(&mut root, overlay);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Failed to read /sandbox/config.json"),
        }

        prepare_config(&mut root);
        apply_env_overrides(&mut root);
        serde_json::from_value(root).context("Failed to deserialise config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TranscodeConfig {
        let json = serde_json::json!({
            "renditionKeys": "proxy,sd,hd",
            "fallbackOrder": "proxy,sd",
            "proxy": { "width": 640, "height": 360, "bitrate": 800000 },
            "sd":    { "width": 1280, "height": 720, "bitrate": 2500000 },
            "hd":    { "width": 1920, "height": 1080, "bitrate": 5000000 },
            "renditionSets": { "clip": "proxy,sd" }
        });
        serde_json::from_value(json).unwrap()
    }

    /// `original` and `transcodeSweep` are named fields, so serde must consume them
    /// before the flattened map — otherwise they would look like rendition keys and
    /// `get_rendition("original")` would hand `scale`-shaped config to the job builder.
    #[test]
    fn reserved_keys_never_land_in_the_rendition_map() {
        let parsed: TranscodeConfig = serde_json::from_value(serde_json::json!({
            "original": { "extension": ".mp4" },
            "transcodeSweep": { "enabled": true },
            "proxy": { "width": 640, "height": 360 }
        }))
        .unwrap();
        assert!(parsed.get_rendition("original").is_none());
        assert!(parsed.get_rendition("transcodeSweep").is_none());
        assert!(parsed.get_rendition("proxy").is_some());
    }

    #[test]
    fn a_category_set_overrides_the_global_keys() {
        assert_eq!(cfg().rendition_keys_for_category(""), ["proxy", "sd", "hd"]);
        assert_eq!(cfg().rendition_keys_for_category("clip"), ["proxy", "sd"]);
        // Unknown category falls back to the global list.
        assert_eq!(cfg().rendition_keys_for_category("nope"), ["proxy", "sd", "hd"]);
    }

    /// Config keys keep their case; categories do not. A `renditionSets` entry with
    /// a capital in it must still be found, or it would silently fall back to the
    /// global list and the sweep would judge that category against the wrong count.
    #[test]
    fn the_category_lookup_ignores_case_on_both_sides() {
        let json = serde_json::json!({
            "renditionKeys": "proxy",
            "renditionSets": { "shortClip": "proxy,sd" },
            "proxy": { "width": 640, "height": 360 },
            "sd":    { "width": 1280, "height": 720 }
        });
        let c: TranscodeConfig = serde_json::from_value(json).unwrap();
        for spelling in ["shortClip", "shortclip", "SHORTCLIP"] {
            assert_eq!(
                c.rendition_keys_for_category(spelling),
                ["proxy", "sd"],
                "{spelling} should find the shortClip set"
            );
        }
    }

    /// A rendition left out of `fallbackOrder` must still be reachable, or adding a
    /// rendition and forgetting to list it would make it silently unplayable.
    #[test]
    fn fallback_order_appends_whatever_it_omitted() {
        assert_eq!(cfg().fallback_order_for_category(""), ["proxy", "sd", "hd"]);
    }

    /// `fallbackOrder` is global but categories are not, so an entry the category
    /// does not configure must be dropped rather than offered and then 404'd.
    #[test]
    fn fallback_order_drops_renditions_the_category_lacks() {
        let mut c = cfg();
        c.fallback_order = "proxy,hd,sd".into();
        assert_eq!(c.fallback_order_for_category("clip"), ["proxy", "sd"]);
    }

    #[test]
    fn an_hls_ladder_parses_and_skips_malformed_rungs() {
        let r = RenditionConfig {
            kind: "hls".into(),
            ladder: "640x360@800000, bogus, 1280x720@2500000".into(),
            ..Default::default()
        };
        assert_eq!(
            r.rungs(),
            vec![
                LadderRung { width: 640, height: 360, bitrate: 800_000 },
                LadderRung { width: 1280, height: 720, bitrate: 2_500_000 },
            ]
        );
        assert!(r.is_hls());
        assert_eq!(r.ext(), ".m3u8");
    }

    /// An `hls` entry with no ladder is a configuration people will write; it should
    /// mean one rung, not zero outputs and a job that produces nothing.
    #[test]
    fn an_hls_rendition_without_a_ladder_falls_back_to_its_own_dimensions() {
        let r = RenditionConfig {
            kind: "hls".into(),
            width: Some(1280),
            height: Some(720),
            bitrate: Some(3_000_000),
            ..Default::default()
        };
        assert_eq!(
            r.rungs(),
            vec![LadderRung { width: 1280, height: 720, bitrate: 3_000_000 }]
        );
    }
}
