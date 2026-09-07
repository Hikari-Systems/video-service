//! Turning an upload into something watchable.
//!
//! Two halves that happen at very different times:
//!
//! * **Inside the request** — store the source, probe it, and if a stream copy can
//!   make it playable, do that. Bounded work, seconds at most, and it is what
//!   decides whether the viewer sees anything at all before the queue drains.
//! * **Somewhere else, later** — MediaConvert produces the renditions.
//!   [`submit_jobs`] hands them over and returns; [`reconcile_job`] writes the
//!   results back whenever they arrive. Neither blocks the other.

use anyhow::{Context, Result};
use std::path::Path;
use tempfile::Builder as TempBuilder;
use tokio::fs;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::RenditionConfig;
use crate::helpers::playback::ORIGINAL_KEY;
use crate::models::video::{Rendition, SourceProbe, TranscodeJob, VideoRecord};
use crate::services::mediaconvert::{JobState, SubmitRequest};
use crate::state::AppState;

/// Return the file extension from a path/filename string (including the dot).
/// Falls back to `.bin` if no extension is found.
pub fn extension_from_path(path: &str) -> String {
    match path.rfind('.') {
        Some(pos) if pos < path.len() - 1 => format!(".{}", &path[pos + 1..]),
        _ => ".bin".to_string(),
    }
}

/// Which half of the configured renditions a request wants.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Immediate {
    None,
    All,
    Only(Vec<String>),
}

/// Resolved answer to "which renditions does this request ask for?".
///
/// The same two-parameter shape as the image service: an allowlist (`renditions`,
/// which also accepts the historic boolean spellings) and a denylist (`defer`)
/// subtracted from it. `defer` is the last word — naming a key in both is a request
/// to defer it.
///
/// Two things it does **not** mean here, both of which differ from the image service:
///
/// 1. **It does not mean the work happens before the response.** A selection decides
///    what gets *submitted*; MediaConvert decides when it is finished.
/// 2. **It does not govern `original`.** See [`Self::wants`] — the original is
///    produced unless `defer` explicitly names it, whatever the allowlist or
///    `transcode.processing` say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionSelection {
    immediate: Immediate,
    deferred: Vec<String>,
}

impl RenditionSelection {
    pub fn parse(force: Option<&str>, defer: Option<&str>, default_immediate: bool) -> Self {
        let immediate = match force.map(str::trim) {
            None | Some("") => {
                if default_immediate { Immediate::All } else { Immediate::None }
            }
            Some(v) if is_truthy(v) => Immediate::All,
            Some(v) if is_falsey(v) => {
                if default_immediate { Immediate::All } else { Immediate::None }
            }
            Some(v) => Immediate::Only(split_keys(v)),
        };
        Self { immediate, deferred: defer.map(split_keys).unwrap_or_default() }
    }

    /// Every key named in either parameter, for validation against the config.
    pub fn requested_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = match &self.immediate {
            Immediate::Only(keys) => keys.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        };
        keys.extend(self.deferred.iter().map(String::as_str));
        keys
    }

    /// Should this be produced in this pass?
    ///
    /// `original` is deliberately **not** subject to `transcode.processing` or to the
    /// allowlist, and this is the one place the image service's semantics could not
    /// be carried over unchanged.
    ///
    /// There, deferring the original saves real work: it is a re-encode, and a later
    /// `POST .../transcode` will produce it. Here it is a byte copy or a stream copy
    /// — bounded, seconds — and it is the *only* thing that makes an upload playable
    /// before the queue drains. Worse, nothing would ever come back for it: the sweep
    /// submits MediaConvert jobs, it does not run remuxes. Honouring
    /// `processing: "deferred"` for the original therefore meant every upload landed
    /// unplayable and stayed that way until a transcode finished — which is precisely
    /// the gap the original exists to close.
    ///
    /// So it is produced unless `?defer=original` explicitly asks otherwise.
    pub fn wants(&self, key: &str) -> bool {
        if self.deferred.iter().any(|d| d == key) {
            return false;
        }
        if key == ORIGINAL_KEY {
            return true;
        }
        match &self.immediate {
            Immediate::None => false,
            Immediate::All => true,
            Immediate::Only(keys) => keys.iter().any(|k| k == key),
        }
    }

    /// The configured renditions this selection asks for, in category order.
    pub fn chosen(&self, rendition_keys: &[String]) -> Vec<String> {
        rendition_keys.iter().filter(|k| self.wants(k)).cloned().collect()
    }
}

fn is_truthy(v: &str) -> bool {
    ["true", "yes", "y", "on", "1", "all"].iter().any(|w| v.eq_ignore_ascii_case(w))
}

fn is_falsey(v: &str) -> bool {
    ["false", "no", "n", "off", "0", "none"].iter().any(|w| v.eq_ignore_ascii_case(w))
}

fn split_keys(raw: &str) -> Vec<String> {
    raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// The S3 key a rendition lands at.
///
/// Deterministic on purpose. MediaConvert names a FILE_GROUP output
/// `<destination><input basename><nameModifier><ext>`, so pointing the destination
/// at the bucket root and setting the modifier to `-<rendition>` reproduces exactly
/// this string — which means a completion can be written back without listing the
/// bucket or parsing a job's output details.
pub fn output_key(category: &str, id: Uuid, rendition: &str, cfg: &RenditionConfig) -> String {
    format!("{}-{}-{}{}", category, id, rendition, cfg.ext())
}

/// The key of the playable original derivative.
pub fn original_key(category: &str, id: Uuid, ext: &str) -> String {
    format!("{}-{}-original{}", category, id, ext)
}

fn category_of(record: &VideoRecord) -> String {
    record
        .category
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or("video")
        .to_lowercase()
}

/// Store the upload, work out what it is, and make it playable if a byte copy can.
///
/// Deliberately never fails because of the probe. An upload that cannot be probed is
/// still an upload; storing it and degrading the fallback beats rejecting the
/// request because ffprobe was missing from the image.
// Deliberately wide, and deliberately the same shape as the image service's
// `process_image`: the two are read side by side whenever this service's behaviour
// is compared against that one, and a struct here for the sake of an arity lint
// would break that correspondence for no reader's benefit.
#[allow(clippy::too_many_arguments)]
pub async fn process_video(
    local_source_path: &Path,
    source_extension: &str,
    source_mime_type: &str,
    category: &str,
    url: Option<&str>,
    selection: &RenditionSelection,
    to_overwrite: Option<VideoRecord>,
    state: &AppState,
) -> Result<VideoRecord> {
    let base = to_overwrite.unwrap_or_default();
    let id = base.id.unwrap_or_else(Uuid::new_v4);
    let category_lc = category.to_lowercase();

    // Probe first, before anything — including the upload — decides what to do with
    // the file. The declared multipart content type is not trustworthy (an uploader
    // that sets no part type leaves `application/octet-stream`, and that is what
    // would otherwise be stamped on the stored object), and for a source we intend to
    // serve directly, the stored content type is what a browser will act on.
    let probe = match state.probe.probe(local_source_path).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!("ffprobe failed for {:?}: {e:#} — playback will fall back on the extension", local_source_path);
            None
        }
    };

    let stored_mime = probe
        .as_ref()
        .and_then(|p| mime_for_container(&p.container))
        .unwrap_or(source_mime_type);

    let s3_path = format!("{}-{}{}", category_lc, id, source_extension);
    info!("S3 upload start: {} → {} ({})", local_source_path.display(), s3_path, stored_mime);
    state
        .s3
        .save(local_source_path, &s3_path, stored_mime)
        .await
        .context("Failed to upload source to S3")?;
    info!("S3 upload done: {}", s3_path);

    let mut record = VideoRecord {
        id: Some(id),
        category: Some(category_lc.clone()),
        source_url: url.map(str::to_string),
        downloaded_s3_path: Some(s3_path),
        probe: probe.clone(),
        ..base
    };

    // The original derivative: the rung that makes the difference between "wait for
    // the queue" and "plays now".
    if selection.wants(ORIGINAL_KEY) {
        match prepare_original(local_source_path, &category_lc, id, probe.as_ref(), state).await {
            Ok(Some(key)) => record.original_s3_path = Some(key),
            Ok(None) => debug!("{id}: no playable original is possible from this source"),
            Err(e) => warn!("{id}: could not produce a playable original: {e:#}"),
        }
    } else {
        debug!("{id}: original deferred by request");
    }

    let record = state
        .backend
        .upsert(record)
        .await
        .context("Failed to upsert video after upload")?;

    // Renditions are somebody else's problem from here — either this call submits
    // the jobs, or the sweep picks the record up and does it.
    let rendition_keys = state.config.transcode.rendition_keys_for_category(&category_lc);
    let wanted = selection.chosen(&rendition_keys);
    if wanted.is_empty() {
        return Ok(record);
    }

    submit_jobs(record, &wanted, state).await
}

/// The content type an object should carry, given what the probe found in it.
///
/// Derived from the container rather than from what the uploader declared, because
/// the declared type is frequently `application/octet-stream` and a browser asked to
/// play that may simply refuse. `None` means the probe found nothing recognisable
/// and the declared type is the best remaining guess.
fn mime_for_container(container: &str) -> Option<&'static str> {
    match container {
        "mp4" | "m4v" => Some("video/mp4"),
        "webm" | "matroska" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        _ => None,
    }
}

/// Produce the playable copy of the source, if one is needed and one is possible.
///
/// Three outcomes, and the middle one is the whole point of doing any local video
/// work at all:
///
/// * **Already progressive** — nothing to do. The raw upload *is* the playable copy,
///   and [`crate::helpers::playback`] serves it directly once the probe has cleared
///   it. The image service copies its source to an `-original` key in this case;
///   doing the same here would duplicate every already-good upload in S3 for no
///   benefit, which on photo-sized objects is invisible and on video-sized ones is
///   a bill.
/// * **Right codecs, wrong container or wrong index position** — stream-copy remux.
///   Seconds of work that converts a file which would have hung a player into one
///   that starts immediately. This is the common case for phone and camera uploads.
/// * **Wrong codecs** — nothing local can help. Return `None`; the viewer waits for
///   MediaConvert, and the playback chain says so rather than handing over a file
///   that will not play.
async fn prepare_original(
    local_source_path: &Path,
    category: &str,
    id: Uuid,
    probe: Option<&SourceProbe>,
    state: &AppState,
) -> Result<Option<String>> {
    let Some(probe) = probe else {
        // Without a probe there is no way to know whether a remux is needed or
        // whether it would even succeed. Leave `original` unset; playback falls back
        // to the raw upload on the extension, flagged unverified.
        return Ok(None);
    };

    if probe.directly_servable() {
        info!(
            "{id}: source is already progressive ({}/{}) — serving it directly, no copy needed",
            probe.container, probe.video_codec
        );
        return Ok(None);
    }

    if probe.remuxable() {
        info!(
            "{id}: source is {}/{} in {} (faststart={}) — remuxing",
            probe.video_codec,
            probe.audio_codec.as_deref().unwrap_or("silent"),
            probe.container,
            probe.faststart
        );
        let remuxed = state.ffmpeg.remux_faststart(local_source_path).await?;
        let original_cfg = &state.config.transcode.original;
        let key = original_key(category, id, &original_cfg.extension);
        let result = state.s3.save(&remuxed, &key, &original_cfg.mime_type).await;
        let _ = fs::remove_file(&remuxed).await;
        result?;
        info!("{id}: remuxed original uploaded → {key}");
        return Ok(Some(key));
    }

    info!(
        "{id}: source is {} in {} — no browser can play it, waiting for MediaConvert",
        probe.video_codec, probe.container
    );
    Ok(None)
}

/// Hand the requested renditions to MediaConvert.
///
/// Split into two jobs when a proxy rendition is configured, because a job's
/// outputs are released together: putting the cheap rendition in with the expensive
/// one would make the viewer wait for both. The proxy job goes in at a higher queue
/// priority so it also *starts* first.
pub async fn submit_jobs(
    mut record: VideoRecord,
    wanted: &[String],
    state: &AppState,
) -> Result<VideoRecord> {
    let Some(id) = record.id else {
        anyhow::bail!("cannot submit a transcode for a record with no id");
    };
    let Some(input_key) = record.downloaded_s3_path.clone() else {
        anyhow::bail!("cannot submit a transcode for video {id}: no source in S3");
    };
    if !state.mediaconvert.is_available() {
        warn!("{id}: MediaConvert is not configured — {wanted:?} will not be produced");
        return Ok(record);
    }

    let category = category_of(&record);
    let cfg = &state.config.transcode;

    let resolve = |names: &[String]| -> Vec<(String, RenditionConfig)> {
        names
            .iter()
            .filter_map(|n| cfg.get_rendition(n).map(|c| (n.clone(), c.clone())))
            .collect()
    };

    let proxy_name = cfg.proxy_rendition.trim();
    let (proxy, rest): (Vec<String>, Vec<String>) = if proxy_name.is_empty() {
        (Vec::new(), wanted.to_vec())
    } else {
        wanted.iter().cloned().partition(|n| n == proxy_name)
    };

    let file_destination = format!("s3://{}/", state.s3.bucket());
    let hls_destination_prefix = format!("s3://{}/{}-{}-", state.s3.bucket(), category, id);

    for (batch, expedited) in [(proxy, true), (rest, false)] {
        let resolved = resolve(&batch);
        if resolved.is_empty() {
            continue;
        }
        let req = SubmitRequest {
            video_id: id,
            category: &category,
            input_uri: state.s3.uri(&input_key),
            file_destination: file_destination.clone(),
            hls_destination_prefix: hls_destination_prefix.clone(),
            renditions: resolved,
            expedited,
        };
        match state.mediaconvert.submit(req).await {
            Ok(job) => record.merge_job(TranscodeJob {
                id: job.id,
                status: "SUBMITTED".to_string(),
                renditions: job.renditions,
                submitted_at: chrono::Utc::now(),
                percent_complete: None,
                error: None,
            }),
            // One batch failing must not lose the other's job id — that job is
            // running and billing whether or not this call returned cleanly.
            Err(e) => warn!("{id}: MediaConvert submission failed for {batch:?}: {e:#}"),
        }
    }

    state
        .backend
        .upsert(record)
        .await
        .context("Failed to upsert video after submitting transcode jobs")
}

/// Apply a job's current state to a record.
///
/// On completion each rendition the job was asked for is confirmed present in S3
/// before it is written into the record. The key is computed rather than read back
/// from the job, so this is the check that the computation was right — a rendition
/// recorded but absent would be a signed URL to a 404, which is the one failure the
/// playback chain cannot detect or fall back from.
pub async fn reconcile_job(
    record: &mut VideoRecord,
    job_state: &JobState,
    state: &AppState,
) -> Result<bool> {
    let Some(id) = record.id else {
        anyhow::bail!("cannot reconcile a record with no id");
    };
    let category = category_of(record);

    let existing = record.jobs.iter().find(|j| j.id == job_state.id).cloned();
    let renditions = existing.as_ref().map(|j| j.renditions.clone()).unwrap_or_default();

    record.merge_job(TranscodeJob {
        id: job_state.id.clone(),
        status: job_state.status.clone(),
        renditions: renditions.clone(),
        submitted_at: existing.map(|j| j.submitted_at).unwrap_or_else(chrono::Utc::now),
        percent_complete: job_state.percent_complete,
        error: job_state.error.clone(),
    });

    if job_state.status != "COMPLETE" {
        if job_state.status == "ERROR" {
            warn!(
                "{id}: MediaConvert job {} failed: {}",
                job_state.id,
                job_state.error.as_deref().unwrap_or("no message")
            );
        }
        return Ok(false);
    }

    let mut wrote = false;
    for name in renditions {
        let Some(cfg) = state.config.transcode.get_rendition(&name) else {
            warn!("{id}: job produced {name}, which is no longer configured — ignoring");
            continue;
        };
        let key = output_key(&category, id, &name, cfg);
        match state.s3.exists(&key).await {
            Ok(true) => {
                record.merge_rendition(Rendition {
                    name: name.clone(),
                    s3_path: key,
                    mime_type: cfg.mime(),
                    kind: cfg.kind.clone(),
                    width: cfg.width,
                    height: cfg.height,
                });
                wrote = true;
                info!("{id}: rendition {name} is ready");
            }
            Ok(false) => warn!(
                "{id}: job {} reported COMPLETE but {key} is not in S3 — not recording {name}",
                job_state.id
            ),
            Err(e) => warn!("{id}: could not confirm {key} exists: {e:#}"),
        }
    }
    Ok(wrote)
}

/// Which renditions this video still lacks, for the sweep.
pub fn missing_renditions(record: &VideoRecord, state: &AppState) -> Vec<String> {
    let category = category_of(record);
    state
        .config
        .transcode
        .rendition_keys_for_category(&category)
        .into_iter()
        .filter(|k| record.rendition(k).is_none())
        .collect()
}

/// Pull a video in from a URL, then run it through [`process_video`].
pub async fn ingest_from_url(
    record: VideoRecord,
    selection: &RenditionSelection,
    state: &AppState,
) -> Result<VideoRecord> {
    let src_url = record
        .source_url
        .clone()
        .filter(|s| !s.is_empty())
        .context("no sourceUrl to ingest from")?;

    let ext = {
        let parsed = url::Url::parse(&src_url)
            .unwrap_or_else(|_| url::Url::parse("http://x/").unwrap());
        extension_from_path(parsed.path())
    };
    let category = category_of(&record);

    let tmp = TempBuilder::new()
        .suffix(&ext)
        .tempfile()
        .context("Failed to create temp file for source download")?;
    let (_, tmp_path) = tmp.keep()?;

    let downloaded = state
        .downloader
        .download(&src_url, &tmp_path)
        .await
        .context("Failed to download video from source URL")?;

    info!(
        "ingested {} ({} bytes, {}) for category {}",
        src_url, downloaded.bytes, downloaded.mime_type, category
    );

    let result = process_video(
        &downloaded.local_path,
        &ext,
        &downloaded.mime_type,
        &category,
        Some(&src_url),
        selection,
        Some(record),
        state,
    )
    .await;

    let _ = fs::remove_file(&tmp_path).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 3] = ["proxy", "sd", "hd"];

    fn keys() -> Vec<String> {
        KEYS.iter().map(|s| s.to_string()).collect()
    }

    /// Everything the selection would produce, original included, in config order.
    fn produced(sel: &RenditionSelection) -> Vec<&'static str> {
        std::iter::once(ORIGINAL_KEY)
            .chain(KEYS)
            .filter(|k| sel.wants(k))
            .collect()
    }

    /// The default shipped configuration is `processing: "deferred"`, and this is the
    /// case that matters most: an upload with no query parameters at all must still
    /// come back playable. Deferring the original here would leave every upload
    /// unwatchable until a transcode finished, with nothing scheduled to fix it.
    #[test]
    fn the_original_survives_the_deferred_default() {
        let sel = RenditionSelection::parse(None, None, false);
        assert_eq!(produced(&sel), ["original"], "deferred must not defer the original");
        assert!(sel.chosen(&keys()).is_empty(), "but no renditions are submitted");

        assert_eq!(
            produced(&RenditionSelection::parse(None, None, true)),
            ["original", "proxy", "sd", "hd"]
        );
    }

    #[test]
    fn boolean_spellings_are_understood() {
        for yes in ["TRUE", "true", "yes", "Y", "on", "1", "all"] {
            assert_eq!(
                produced(&RenditionSelection::parse(Some(yes), None, false)),
                ["original", "proxy", "sd", "hd"],
                "{yes} should mean everything"
            );
        }
        // A falsey value turns the renditions off; it is not a way to ask for an
        // unplayable upload.
        for no in ["false", "NO", "n", "off", "0", "none"] {
            assert_eq!(
                produced(&RenditionSelection::parse(Some(no), None, false)),
                ["original"],
                "{no} should mean no renditions"
            );
        }
    }

    /// The shape a client actually uses: keep the cheap original inline, leave the
    /// expensive ladder to the queue.
    #[test]
    fn an_explicit_list_selects_renditions_only() {
        let sel = RenditionSelection::parse(Some("proxy"), None, false);
        assert_eq!(produced(&sel), ["original", "proxy"]);
        assert_eq!(sel.chosen(&keys()), ["proxy"]);

        // Naming it explicitly is accepted and changes nothing — it was already on.
        let named = RenditionSelection::parse(Some("original,proxy"), None, false);
        assert_eq!(produced(&named), ["original", "proxy"]);
    }

    #[test]
    fn defer_subtracts_and_has_the_last_word() {
        assert_eq!(
            produced(&RenditionSelection::parse(Some("true"), Some("hd"), false)),
            ["original", "proxy", "sd"]
        );
        // Named in both: deferring is the more specific instruction.
        assert_eq!(
            produced(&RenditionSelection::parse(Some("proxy,hd"), Some("hd"), false)),
            ["original", "proxy"]
        );
        // `defer` is the only way to skip the local remux, and it must not take the
        // renditions with it.
        let sel = RenditionSelection::parse(None, Some("original"), true);
        assert_eq!(produced(&sel), ["proxy", "sd", "hd"]);
    }

    #[test]
    fn requested_keys_covers_both_params_for_validation() {
        let sel = RenditionSelection::parse(Some("proxy, bogus"), Some("nope"), false);
        assert_eq!(sel.requested_keys(), ["proxy", "bogus", "nope"]);
        assert!(RenditionSelection::parse(Some("true"), None, false).requested_keys().is_empty());
    }

    /// The key has to match what MediaConvert will actually write, because the
    /// reconcile computes it rather than reading it back from the job.
    #[test]
    fn the_output_key_matches_the_name_modifier_convention() {
        let id = Uuid::nil();
        let mp4 = RenditionConfig { kind: "progressive".into(), ..Default::default() };
        assert_eq!(
            output_key("clip", id, "proxy", &mp4),
            format!("clip-{id}-proxy.mp4")
        );

        let hls = RenditionConfig { kind: "hls".into(), ..Default::default() };
        assert_eq!(
            output_key("clip", id, "hls", &hls),
            format!("clip-{id}-hls.m3u8")
        );
    }

    /// Only the remux writes this key now — an already-progressive source is served
    /// where it lies rather than duplicated — so the extension comes from
    /// `transcode.original`, which the remux muxer fixes at MP4.
    #[test]
    fn the_original_key_is_the_remux_destination() {
        let id = Uuid::nil();
        assert_eq!(original_key("clip", id, ".mp4"), format!("clip-{id}-original.mp4"));
    }

    /// The declared multipart type is routinely `application/octet-stream`, and for a
    /// source we intend to serve directly that is what a browser would be handed.
    /// The probe knows better.
    #[test]
    fn the_stored_content_type_comes_from_the_container_not_the_uploader() {
        assert_eq!(mime_for_container("mp4"), Some("video/mp4"));
        assert_eq!(mime_for_container("m4v"), Some("video/mp4"));
        assert_eq!(mime_for_container("webm"), Some("video/webm"));
        assert_eq!(mime_for_container("matroska"), Some("video/webm"));
        assert_eq!(mime_for_container("mov"), Some("video/quicktime"));
        // Nothing recognised: the declared type is the best remaining guess.
        assert_eq!(mime_for_container("avi"), None);
        assert_eq!(mime_for_container(""), None);
    }

    #[test]
    fn extensions_come_off_paths_with_a_sane_fallback() {
        assert_eq!(extension_from_path("clip.mp4"), ".mp4");
        assert_eq!(extension_from_path("/a/b/clip.MOV"), ".MOV");
        assert_eq!(extension_from_path("noextension"), ".bin");
        assert_eq!(extension_from_path("trailing."), ".bin");
    }
}
