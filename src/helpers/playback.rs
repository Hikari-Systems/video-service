//! Deciding what a viewer can actually watch, right now.
//!
//! The image service resolves a request for a size by walking a chain — exact
//! variant, then the original, then the raw upload — and redirecting to whatever it
//! finds first. It can do that blindly because every rung of that chain is a file a
//! browser will render: an upload that is a JPEG stays a JPEG.
//!
//! Video breaks that assumption in two independent ways, and this module exists to
//! stop the blind redirect from becoming a broken `<video>` element:
//!
//! 1. **The codec or container may be undecodable.** ProRes, MKV, AVI, and (outside
//!    Safari) HEVC are ordinary things for a person to upload and impossible for
//!    most browsers to play. No amount of falling back helps; the viewer has to wait
//!    for MediaConvert.
//! 2. **The index may be at the end of the file.** An MP4 that is *entirely*
//!    decodable still will not start until the player has the `moov` atom, and most
//!    phone and camera muxers write it last. The player pulls the whole file before
//!    frame one, which on anything large is indistinguishable from a hang.
//!
//! So the chain here is a *checked* one. Every rung is something we have established
//! will play — either because MediaConvert produced it, or because
//! [`SourceProbe::directly_servable`] says the upload can be served as-is. When no
//! rung qualifies, the answer is an explicit "not yet" rather than a redirect: a
//! client that is told `pending` shows a spinner and polls, whereas a client handed
//! a URL to an unplayable file has no way to tell "broken" from "not finished".

use serde::Serialize;
use tracing::debug;

use crate::config::AppConfig;
use crate::models::video::{Rendition, VideoRecord};
use crate::services::cloudfront::CloudfrontService;

/// The reserved name for the playable copy of the source. Like the image service's
/// `original`, it is a named config field rather than a rendition key, so it can
/// never appear in `renditionKeys` or a `renditionSets` entry.
pub const ORIGINAL_KEY: &str = "original";

/// The reserved name for the raw upload, served only when the probe clears it.
pub const SOURCE_KEY: &str = "source";

/// A URL that will play, and what it turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub struct Playable {
    pub url: String,
    /// Which rung answered: a rendition name, `original`, or `source`.
    pub rendition: String,
    /// True when this is the rendition that was actually asked for.
    pub exact: bool,
    /// `progressive` or `hls`.
    pub kind: String,
    /// For HLS: the path prefix the segments live under, which is what a signed
    /// cookie has to cover. A signed *URL* only authorises the manifest request;
    /// the segment requests that follow carry no query string and would 403.
    pub cookie_prefix: Option<String>,
    /// False when the rung was chosen on the strength of the file extension because
    /// no probe was stored. It may well play; nothing has confirmed that it will.
    pub verified: bool,
}

/// Why there is nothing to play yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// A MediaConvert job is running. This is the ordinary case, and it is temporary.
    Transcoding,
    /// The upload cannot be decoded by a browser and no rendition has landed yet.
    /// Also temporary, but nothing can be shown in the meantime.
    SourceNotPlayable,
    /// Nothing has been submitted. Either the upload deferred it and the sweep has
    /// not run, or submission failed.
    NotStarted,
    /// The record exists but has no source at all — nothing was ever uploaded.
    NoSource,
}

impl NotReady {
    pub fn as_str(self) -> &'static str {
        match self {
            NotReady::Transcoding => "transcoding",
            NotReady::SourceNotPlayable => "source_not_playable",
            NotReady::NotStarted => "not_started",
            NotReady::NoSource => "no_source",
        }
    }
}

/// What the client needs in order to choose between playing and waiting.
#[derive(Debug, Clone, Serialize)]
pub struct PlaybackStatus {
    /// `ready` or `pending`.
    pub state: String,
    /// Everything playable right now, best first — rendition names plus the
    /// reserved `original` / `source` rungs when they qualify.
    pub ready: Vec<String>,
    /// Configured renditions that have not landed yet.
    pub pending: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(rename = "percentComplete", skip_serializing_if = "Option::is_none")]
    pub percent_complete: Option<i32>,
}

/// Extensions whose container a browser will attempt, used only when no probe was
/// stored.
///
/// Consulted for the same reason the image service consults extensions: the declared
/// content type does not survive the round trip through S3 and back, and an uploader
/// that sets no part type leaves `application/octet-stream` behind. Deliberately
/// narrow, and never enough on its own to mark a rung `verified` — the extension can
/// witness the container but says nothing about where the index sits.
///
/// **This list must agree with `ffprobe::PLAYABLE_CONTAINER`.** `.mov` is absent from
/// both, and for the same reason: its codecs are usually fine but Firefox will not
/// reliably open the container, so a MOV is something to remux rather than something
/// to serve. Letting it in here would have the unprobed path hand out a file the
/// probed path had already decided against.
fn playable_by_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".mp4", ".m4v", ".webm"].iter().any(|ext| lower.ends_with(ext))
}

/// The prefix an HLS rendition's segments share with its manifest.
///
/// `clip-<id>-hls.m3u8` → `clip-<id>-hls`, which is what the cookie policy has to
/// cover for the segment requests to be authorised alongside the manifest.
fn hls_prefix(s3_path: &str) -> String {
    match s3_path.rfind('.') {
        Some(pos) => s3_path[..pos].to_string(),
        None => s3_path.to_string(),
    }
}

fn playable_from_rendition(r: &Rendition, exact: bool, cf: &CloudfrontService) -> anyhow::Result<Playable> {
    Ok(Playable {
        url: cf.get_signed_url(&r.s3_path)?,
        rendition: r.name.clone(),
        exact,
        kind: r.kind.clone(),
        cookie_prefix: r.is_hls().then(|| hls_prefix(&r.s3_path)),
        verified: true,
    })
}

/// Walk the checked fallback chain and return the first rung that will play.
///
/// Order, and why:
///
/// 1. **The exact rendition asked for.** Obviously.
/// 2. **The category's other renditions, cheapest first.** A viewer waiting on `hd`
///    would rather watch `proxy` now than nothing, and the player can upgrade later.
/// 3. **The faststart remux.** Written only when the codecs were fine and something
///    else was not, and only when the remux succeeded — so the bytes behind it are
///    known to play, and it needs no further checking here.
/// 4. **The raw upload, if the probe cleared it.** Not a rare rung: an upload that
///    arrived already progressive is never copied anywhere, so this is what answers
///    for it.
/// 5. **The caller's own `sourceUrl`.** Handing back the URL we were given is no
///    worse than the caller's starting position.
pub fn resolve(
    video: &VideoRecord,
    requested: &str,
    config: &AppConfig,
    cf: &CloudfrontService,
) -> anyhow::Result<Result<Playable, NotReady>> {
    let category = video
        .category
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or("video")
        .to_lowercase();

    // 1. The exact rendition.
    if let Some(r) = video.rendition(requested) {
        debug!("playback {:?}: exact rendition {requested}", video.id);
        return Ok(Ok(playable_from_rendition(r, true, cf)?));
    }

    // 2. The category's other renditions, in fallback order.
    for key in config.transcode.fallback_order_for_category(&category) {
        if key == requested {
            continue;
        }
        if let Some(r) = video.rendition(&key) {
            debug!("playback {:?}: {requested} not ready, falling back to {key}", video.id);
            return Ok(Ok(playable_from_rendition(r, false, cf)?));
        }
    }

    // 3. The remuxed original. Its existence is the guarantee — nothing writes it
    //    unless the remux ran and succeeded.
    if let Some(path) = &video.original_s3_path {
        debug!("playback {:?}: falling back to the playable original", video.id);
        return Ok(Ok(Playable {
            url: cf.get_signed_url(path)?,
            rendition: ORIGINAL_KEY.to_string(),
            exact: false,
            kind: "progressive".to_string(),
            cookie_prefix: None,
            verified: true,
        }));
    }

    // 4. The raw upload — only when something has actually cleared it.
    if let Some(path) = &video.downloaded_s3_path {
        let (servable, verified) = match &video.probe {
            Some(p) => (p.directly_servable(), true),
            // No probe means ffprobe was unavailable or failed. The extension is a
            // weaker witness, and it cannot see where the index sits — but a file
            // that might play beats a spinner that definitely does not, so long as
            // the answer is not dressed up as verified.
            None => (playable_by_extension(path), false),
        };
        if servable {
            debug!("playback {:?}: serving the raw upload (verified={verified})", video.id);
            return Ok(Ok(Playable {
                url: cf.get_signed_url(path)?,
                rendition: SOURCE_KEY.to_string(),
                exact: false,
                kind: "progressive".to_string(),
                cookie_prefix: None,
                verified,
            }));
        }
    }

    // 5. Whatever the caller gave us in the first place.
    if let Some(src) = video.source_url.as_deref().filter(|s| !s.is_empty()) {
        debug!("playback {:?}: falling back to the caller's sourceUrl", video.id);
        return Ok(Ok(Playable {
            url: src.to_string(),
            rendition: SOURCE_KEY.to_string(),
            exact: false,
            kind: "progressive".to_string(),
            cookie_prefix: None,
            verified: false,
        }));
    }

    Ok(Err(why_not_ready(video)))
}

/// Distinguish "still working" from "nothing can be shown", so the client can say
/// something true while it waits.
fn why_not_ready(video: &VideoRecord) -> NotReady {
    if video.downloaded_s3_path.is_none() && video.source_url.is_none() {
        return NotReady::NoSource;
    }
    if video.has_in_flight_job() {
        return NotReady::Transcoding;
    }
    match &video.probe {
        Some(p) if !p.browser_playable => NotReady::SourceNotPlayable,
        _ => NotReady::NotStarted,
    }
}

/// Build the status block returned alongside (or instead of) a URL.
pub fn status(video: &VideoRecord, config: &AppConfig) -> PlaybackStatus {
    let category = video
        .category
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or("video")
        .to_lowercase();
    let configured = config.transcode.fallback_order_for_category(&category);

    let mut ready: Vec<String> = configured
        .iter()
        .filter(|k| video.rendition(k).is_some())
        .cloned()
        .collect();
    if video.original_s3_path.is_some() {
        ready.push(ORIGINAL_KEY.to_string());
    } else if video
        .downloaded_s3_path
        .as_deref()
        .is_some_and(|p| video.probe.as_ref().map_or(playable_by_extension(p), |x| x.directly_servable()))
    {
        ready.push(SOURCE_KEY.to_string());
    }

    let pending: Vec<String> = configured
        .into_iter()
        .filter(|k| video.rendition(k).is_none())
        .collect();

    let percent = video
        .in_flight_jobs()
        .filter_map(|j| j.percent_complete)
        .max();

    if ready.is_empty() {
        let reason = why_not_ready(video);
        PlaybackStatus {
            state: "pending".to_string(),
            ready,
            pending,
            reason: Some(reason.as_str().to_string()),
            percent_complete: percent,
        }
    } else {
        PlaybackStatus {
            state: "ready".to_string(),
            ready,
            pending,
            // Still worth saying *why* things are outstanding while something plays.
            reason: video.has_in_flight_job().then(|| NotReady::Transcoding.as_str().to_string()),
            percent_complete: percent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CloudfrontConfig;
    use crate::models::video::{SourceProbe, TranscodeJob};
    use chrono::Utc;

    fn config() -> AppConfig {
        let json = serde_json::json!({
            "transcode": {
                "renditionKeys": "proxy,sd,hd",
                "fallbackOrder": "proxy,sd,hd",
                "proxy": { "width": 640,  "height": 360 },
                "sd":    { "width": 1280, "height": 720 },
                "hd":    { "width": 1920, "height": 1080 }
            }
        });
        serde_json::from_value(json).unwrap()
    }

    /// Unsigned, so the assertions can look at the path rather than a signature.
    fn cf() -> CloudfrontService {
        CloudfrontService::new(&CloudfrontConfig {
            url: "https://cdn.example.com".into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn rendition(name: &str, kind: &str) -> Rendition {
        Rendition {
            name: name.into(),
            s3_path: format!("clip-1-{name}{}", if kind == "hls" { ".m3u8" } else { ".mp4" }),
            mime_type: "video/mp4".into(),
            kind: kind.into(),
            width: None,
            height: None,
        }
    }

    fn base() -> VideoRecord {
        VideoRecord {
            id: Some(uuid::Uuid::nil()),
            category: Some("clip".into()),
            downloaded_s3_path: Some("clip-1.mov".into()),
            ..Default::default()
        }
    }

    fn resolve_ok(v: &VideoRecord, want: &str) -> Result<Playable, NotReady> {
        resolve(v, want, &config(), &cf()).unwrap()
    }

    #[test]
    fn the_exact_rendition_wins() {
        let mut v = base();
        v.merge_rendition(rendition("proxy", "progressive"));
        v.merge_rendition(rendition("hd", "progressive"));
        let got = resolve_ok(&v, "hd").unwrap();
        assert_eq!(got.rendition, "hd");
        assert!(got.exact);
    }

    /// The point of the fallback: something to watch while the expensive rendition
    /// is still in the queue.
    #[test]
    fn a_missing_rendition_falls_back_to_a_cheaper_one() {
        let mut v = base();
        v.merge_rendition(rendition("proxy", "progressive"));
        let got = resolve_ok(&v, "hd").unwrap();
        assert_eq!(got.rendition, "proxy");
        assert!(!got.exact);
        assert!(got.verified);
    }

    /// The whole reason this module is not the image service's blind chain: a
    /// ProRes upload with nothing transcoded yet must NOT be handed to a player.
    #[test]
    fn an_unplayable_source_is_pending_not_a_redirect() {
        let mut v = base();
        v.probe = Some(SourceProbe {
            container: "mov".into(),
            video_codec: "prores".into(),
            browser_playable: false,
            faststart: true,
            ..Default::default()
        });
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::SourceNotPlayable));

        v.jobs.push(TranscodeJob {
            id: "j1".into(),
            status: "PROGRESSING".into(),
            renditions: vec!["hd".into()],
            submitted_at: Utc::now(),
            percent_complete: Some(42),
            error: None,
        });
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::Transcoding));
    }

    /// A decodable-but-tail-indexed MP4 is the sneaky one. It is not an error and it
    /// is not playable; serving it would look like a hang.
    #[test]
    fn a_tail_indexed_upload_is_not_served_as_a_fallback() {
        let mut v = base();
        v.downloaded_s3_path = Some("clip-1.mp4".into());
        v.probe = Some(SourceProbe {
            container: "mp4".into(),
            video_codec: "h264".into(),
            browser_playable: true,
            faststart: false,
            ..Default::default()
        });
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::NotStarted));

        // Once the remux has landed, the original rung answers.
        v.original_s3_path = Some("clip-1-original.mp4".into());
        let got = resolve_ok(&v, "hd").unwrap();
        assert_eq!(got.rendition, ORIGINAL_KEY);
        assert!(got.verified);
    }

    /// A source that is genuinely progressive needs no transcode to be watchable.
    #[test]
    fn a_faststart_playable_upload_is_served_directly() {
        let mut v = base();
        v.downloaded_s3_path = Some("clip-1.mp4".into());
        v.probe = Some(SourceProbe {
            browser_playable: true,
            faststart: true,
            ..Default::default()
        });
        let got = resolve_ok(&v, "hd").unwrap();
        assert_eq!(got.rendition, SOURCE_KEY);
        assert!(got.verified);
    }

    /// The unprobed path must not offer a container the probed path rejects. A MOV
    /// is remuxed, never served, and this is the rung that would quietly disagree.
    #[test]
    fn the_extension_fallback_agrees_with_the_probe_about_mov() {
        assert!(!playable_by_extension("clip-1.mov"));
        assert!(playable_by_extension("clip-1.mp4"));
        assert!(playable_by_extension("clip-1.WEBM"));

        let mut v = base(); // base() uploads a .mov
        v.probe = None;
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::NotStarted));
    }

    /// If ffprobe was unavailable the extension is the only witness left. Serving on
    /// it is defensible; calling it verified is not.
    #[test]
    fn an_unprobed_upload_falls_back_on_the_extension_but_is_not_verified() {
        let mut v = base();
        v.downloaded_s3_path = Some("clip-1.mp4".into());
        v.probe = None;
        let got = resolve_ok(&v, "hd").unwrap();
        assert_eq!(got.rendition, SOURCE_KEY);
        assert!(!got.verified);

        // An extension no browser opens gets no such benefit of the doubt.
        v.downloaded_s3_path = Some("clip-1.mkv".into());
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::NotStarted));
    }

    /// HLS needs the segment prefix, because a signed URL authorises the manifest
    /// request and nothing else.
    #[test]
    fn an_hls_rendition_reports_the_prefix_its_cookies_must_cover() {
        let mut v = base();
        v.merge_rendition(rendition("hls", "hls"));
        let got = resolve_ok(&v, "hls").unwrap();
        assert_eq!(got.kind, "hls");
        assert_eq!(got.cookie_prefix.as_deref(), Some("clip-1-hls"));
    }

    #[test]
    fn status_separates_what_plays_from_what_is_outstanding() {
        let mut v = base();
        v.merge_rendition(rendition("proxy", "progressive"));
        v.jobs.push(TranscodeJob {
            id: "j1".into(),
            status: "PROGRESSING".into(),
            renditions: vec!["sd".into(), "hd".into()],
            submitted_at: Utc::now(),
            percent_complete: Some(10),
            error: None,
        });
        let s = status(&v, &config());
        assert_eq!(s.state, "ready");
        assert_eq!(s.ready, ["proxy"]);
        assert_eq!(s.pending, ["sd", "hd"]);
        assert_eq!(s.percent_complete, Some(10));
        assert_eq!(s.reason.as_deref(), Some("transcoding"));
    }

    /// A record with nothing uploaded at all is a different problem from one that is
    /// merely still working, and the client should be able to tell them apart.
    #[test]
    fn an_empty_record_reports_no_source() {
        let v = VideoRecord { id: Some(uuid::Uuid::nil()), ..Default::default() };
        assert_eq!(resolve_ok(&v, "hd"), Err(NotReady::NoSource));

        let s = status(&v, &config());
        assert_eq!(s.state, "pending");
        assert_eq!(s.reason.as_deref(), Some("no_source"));
        assert!(s.ready.is_empty());
    }
}
