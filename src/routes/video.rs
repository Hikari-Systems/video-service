use actix_multipart::Multipart;
use actix_web::cookie::{Cookie, SameSite};
use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use std::io::Write;
use tempfile::Builder as TempBuilder;
use tracing::{debug, error, warn};

use crate::helpers::playback::{self, Playable, ORIGINAL_KEY};
use crate::helpers::transcode::{
    extension_from_path, ingest_from_url, missing_renditions, process_video, submit_jobs,
    RenditionSelection,
};
use crate::models::video::VideoDescriptor;
use crate::state::AppState;

/// Multipart field names accepted for the upload. `video` is the documented one;
/// `file` is accepted because it is what a generic uploader sends, and rejecting a
/// perfectly good upload over a field name is a poor trade.
const UPLOAD_FIELDS: [&str; 2] = ["video", "file"];

pub fn configure(cfg: &mut web::ServiceConfig) {
    // The literal-prefixed and deeper routes come first, so the `{id}` and
    // `{category}` wildcards cannot swallow them.
    cfg.route("/api/video/{id}/transcode", web::post().to(transcode_handler))
        .route("/api/video/{category}/url", web::post().to(ingest_url))
        .route("/api/video/{id}/status", web::get().to(get_status))
        .route("/api/video/{id}/cookies/{rendition}", web::get().to(get_cookies))
        .route("/api/video/r/{id}/{rendition}", web::get().to(get_redirect))
        .route("/api/video/s/{id}/{rendition}", web::get().to(get_signed_url))
        .route("/api/video/{id}", web::get().to(get_video))
        .route("/api/video/{category}", web::post().to(upload_video));
}

/// How long a client should wait before asking again about a pending video.
///
/// Sent as `Retry-After` on every 202. Without it a polling client picks its own
/// interval, which in practice means one second, which in practice means a thousand
/// requests while a two-minute transcode runs.
const RETRY_AFTER_SECONDS: u32 = 5;

/// GET /api/video/:id — descriptor JSON, including playback status.
async fn get_video(path: web::Path<String>, state: web::Data<AppState>) -> HttpResponse {
    let id = path.into_inner();
    debug!("Get by id: {}", id);

    match state.backend.get(&id).await {
        Err(e) => {
            error!("Error getting video id={}: {}", id, e);
            HttpResponse::InternalServerError().finish()
        }
        Ok(None) => HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Ok(Some(record)) => {
            let status = playback::status(&record, &state.config);
            // Best effort: a descriptor is still useful when signing fails, and the
            // playback block already says what is and is not ready.
            let original_file_url = record
                .original_s3_path
                .as_deref()
                .or(record.downloaded_s3_path.as_deref())
                .and_then(|p| state.cf.get_signed_url(p).ok())
                .or_else(|| record.source_url.clone());

            HttpResponse::Ok().json(VideoDescriptor {
                record,
                original_file_url,
                playback: status,
            })
        }
    }
}

/// GET /api/video/:id/status — playback status on its own, for polling.
async fn get_status(path: web::Path<String>, state: web::Data<AppState>) -> HttpResponse {
    let id = path.into_inner();
    match state.backend.get(&id).await {
        Err(e) => {
            error!("Error getting video id={}: {}", id, e);
            HttpResponse::InternalServerError().finish()
        }
        Ok(None) => HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Ok(Some(record)) => {
            let status = playback::status(&record, &state.config);
            let mut res = if status.state == "ready" {
                HttpResponse::Ok()
            } else {
                let mut r = HttpResponse::Accepted();
                r.append_header(("Retry-After", RETRY_AFTER_SECONDS.to_string()));
                r
            };
            res.json(status)
        }
    }
}

/// GET /api/video/r/:id/:rendition — redirect to something playable.
///
/// The difference from the image service's equivalent is the 202. There, a
/// fallback always exists, so the only outcomes are a redirect and a 404. Here a
/// video can exist, be perfectly valid, and have nothing playable yet — and
/// redirecting to an undecodable source would hand the client a broken player with
/// no way to tell that waiting would have fixed it. A 202 with a status body says
/// "not yet, ask again", which a client can act on.
async fn get_redirect(
    path: web::Path<(String, String)>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let (id, rendition) = path.into_inner();
    debug!("Get redirect for video {} rendition {}", id, rendition);

    match resolve(&id, &rendition, &state).await {
        Resolution::Error => HttpResponse::InternalServerError().finish(),
        Resolution::Missing => HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Resolution::Pending(body) => pending_response(body),
        Resolution::Ready(playable, _) => HttpResponse::Found()
            .append_header(("Location", playable.url))
            // The chosen rendition is not always the one asked for, and a client
            // that cares (to decide whether to re-request later, say) cannot see it
            // from a 302 otherwise.
            .append_header(("X-Video-Rendition", playable.rendition))
            .finish(),
    }
}

/// GET /api/video/s/:id/:rendition — the same resolution, as JSON.
async fn get_signed_url(
    path: web::Path<(String, String)>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let (id, rendition) = path.into_inner();
    debug!("Get signed url for video {} rendition {}", id, rendition);

    match resolve(&id, &rendition, &state).await {
        Resolution::Error => HttpResponse::InternalServerError().finish(),
        Resolution::Missing => HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Resolution::Pending(body) => pending_response(body),
        Resolution::Ready(playable, status) => HttpResponse::Ok().json(serde_json::json!({
            "url": playable.url,
            "rendition": playable.rendition,
            "exact": playable.exact,
            "kind": playable.kind,
            "verified": playable.verified,
            // Present for HLS only. A signed URL authorises the manifest request and
            // nothing else, so the client has to collect cookies before the player
            // starts fetching segments.
            "cookiePath": playable.cookie_prefix.as_ref()
                .map(|_| format!("/api/video/{}/cookies/{}", id, playable.rendition)),
            "playback": status,
        })),
    }
}

/// GET /api/video/:id/cookies/:rendition — CloudFront signed cookies for HLS.
///
/// Only meaningful for an HLS rendition. The player fetches the manifest and then
/// issues its own requests for each segment, carrying no query string; a signed URL
/// covers none of those. A custom policy over the shared prefix, delivered as
/// cookies, covers the manifest and its segments together.
async fn get_cookies(
    path: web::Path<(String, String)>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let (id, rendition) = path.into_inner();

    let record = match state.backend.get(&id).await {
        Err(e) => {
            error!("Error getting video id={}: {}", id, e);
            return HttpResponse::InternalServerError().finish();
        }
        Ok(None) => return HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Ok(Some(r)) => r,
    };

    let Some(r) = record.rendition(&rendition) else {
        return HttpResponse::NotFound().body(format!("Rendition not ready: {}", rendition));
    };
    if !r.is_hls() {
        return HttpResponse::BadRequest()
            .body(format!("Rendition {} is not HLS; use the signed URL instead", rendition));
    }

    let prefix = match r.s3_path.rfind('.') {
        Some(pos) => &r.s3_path[..pos],
        None => r.s3_path.as_str(),
    };

    match state.cf.get_signed_cookies(prefix) {
        Err(e) => {
            error!("Error signing cookies for id={}: {}", id, e);
            HttpResponse::InternalServerError().finish()
        }
        // Signing disabled — the local/MinIO case, where the objects are public and
        // there is nothing to authorise. Hand back the manifest URL and say so.
        Ok(None) => HttpResponse::Ok().json(serde_json::json!({
            "signed": false,
            "manifestUrl": state.cf.get_url(&r.s3_path),
        })),
        Ok(Some(c)) => {
            let mut res = HttpResponse::Ok();
            for (name, value) in [
                ("CloudFront-Policy", &c.policy),
                ("CloudFront-Signature", &c.signature),
                ("CloudFront-Key-Pair-Id", &c.key_pair_id),
            ] {
                let mut cookie = Cookie::build(name, value.clone())
                    .path(c.path.clone())
                    .secure(true)
                    .http_only(true)
                    .same_site(SameSite::None)
                    .finish();
                if let Some(domain) = &c.domain {
                    cookie.set_domain(domain.clone());
                }
                res.cookie(cookie);
            }
            res.json(serde_json::json!({
                "signed": true,
                "manifestUrl": state.cf.get_url(&r.s3_path),
                "expiresAt": c.expires_at,
                "path": c.path,
            }))
        }
    }
}

enum Resolution {
    Ready(Playable, playback::PlaybackStatus),
    Pending(serde_json::Value),
    Missing,
    Error,
}

async fn resolve(id: &str, rendition: &str, state: &AppState) -> Resolution {
    let record = match state.backend.get(id).await {
        Err(e) => {
            error!("Error getting video id={}: {}", id, e);
            return Resolution::Error;
        }
        Ok(None) => return Resolution::Missing,
        Ok(Some(r)) => r,
    };

    let status = playback::status(&record, &state.config);
    match playback::resolve(&record, rendition, &state.config, &state.cf) {
        Err(e) => {
            error!("Error resolving playback for id={}: {}", id, e);
            Resolution::Error
        }
        Ok(Ok(playable)) => Resolution::Ready(playable, status),
        Ok(Err(reason)) => {
            debug!("video {id}: nothing playable yet ({})", reason.as_str());
            Resolution::Pending(serde_json::json!({
                "id": id,
                "requested": rendition,
                "playback": status,
                "reason": reason.as_str(),
                "retryAfter": RETRY_AFTER_SECONDS,
            }))
        }
    }
}

fn pending_response(body: serde_json::Value) -> HttpResponse {
    HttpResponse::Accepted()
        .append_header(("Retry-After", RETRY_AFTER_SECONDS.to_string()))
        .json(body)
}

/// Resolve the rendition-selection query parameters, rejecting any key the config
/// does not define. Validating up front means a typo is a 400 rather than a 500
/// raised after the source has already been uploaded to S3 — or worse, a
/// MediaConvert job submitted for a rendition that does not exist.
fn selection_from_query(
    force: Option<&String>,
    defer: Option<&String>,
    default_immediate: bool,
    state: &AppState,
) -> Result<RenditionSelection, String> {
    let selection = RenditionSelection::parse(
        force.map(String::as_str),
        defer.map(String::as_str),
        default_immediate,
    );

    for key in selection.requested_keys() {
        if key != ORIGINAL_KEY && state.config.transcode.get_rendition(key).is_none() {
            return Err(format!("Unknown rendition: {}", key));
        }
    }
    Ok(selection)
}

/// POST /api/video/:id/transcode — submit MediaConvert jobs for an existing video.
///
/// Accepts `?renditions=` and `?defer=`. With neither, everything the category's
/// set configures that is not already present is submitted. Returns immediately:
/// the response says what was *submitted*, and the renditions appear when the jobs
/// finish.
async fn transcode_handler(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let id = path.into_inner();

    // This endpoint has always transcoded regardless of `transcode.processing`, so
    // an absent `renditions` means everything.
    let selection =
        match selection_from_query(query.get("renditions"), query.get("defer"), true, &state) {
            Ok(s) => s,
            Err(msg) => {
                error!("Transcode rejected for id={}: {}", id, msg);
                return HttpResponse::BadRequest().body(msg);
            }
        };

    let record = match state.backend.get(&id).await {
        Err(e) => {
            error!("Error getting video id={}: {}", id, e);
            return HttpResponse::InternalServerError().finish();
        }
        Ok(None) => return HttpResponse::NotFound().body(format!("Video not found: {}", id)),
        Ok(Some(r)) => r,
    };

    if record.downloaded_s3_path.is_none() {
        return HttpResponse::NotFound().body(format!("Video {} has no stored source", id));
    }
    if !state.mediaconvert.is_available() {
        return HttpResponse::ServiceUnavailable()
            .body("MediaConvert is not configured (mediaconvert.roleArn is empty)");
    }

    // Default to what is actually outstanding rather than re-submitting the lot: a
    // job for a rendition that already exists is a duplicate bill for a file we
    // have.
    let wanted: Vec<String> = missing_renditions(&record, &state)
        .into_iter()
        .filter(|k| selection.wants(k))
        .collect();

    if wanted.is_empty() {
        debug!("{id}: nothing outstanding to submit");
        return HttpResponse::Ok().json(serde_json::json!({
            "id": id,
            "submitted": Vec::<String>::new(),
            "playback": playback::status(&record, &state.config),
        }));
    }

    match submit_jobs(record, &wanted, &state).await {
        Ok(updated) => HttpResponse::Accepted().json(serde_json::json!({
            "id": id,
            "submitted": wanted,
            "jobs": updated.jobs,
            "playback": playback::status(&updated, &state.config),
        })),
        Err(e) => {
            error!("Error submitting transcode for id={}: {}", id, e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

/// Body of `POST /api/video/:category/url`.
#[derive(serde::Deserialize)]
struct IngestBody {
    url: String,
}

/// POST /api/video/:category/url — pull a video in from a URL rather than a
/// multipart body.
///
/// The same pipeline as an upload once the bytes are local: store, probe, remux if
/// that helps, submit. Worth having as its own route because a video large enough
/// to be worth transcoding is often already sitting somewhere addressable, and
/// round-tripping it through the caller only to post it back is a waste of two
/// transfers.
async fn ingest_url(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    body: web::Json<IngestBody>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let category = path.into_inner();

    if url::Url::parse(&body.url).is_err() {
        return HttpResponse::BadRequest().body(format!("Not a valid URL: {}", body.url));
    }

    let default_immediate = state.config.transcode.processing.trim() != "deferred";
    let selection = match selection_from_query(
        query.get("renditions"),
        query.get("defer"),
        default_immediate,
        &state,
    ) {
        Ok(s) => s,
        Err(msg) => {
            error!("Ingest rejected for category={}: {}", category, msg);
            return HttpResponse::BadRequest().body(msg);
        }
    };

    let seed = crate::models::video::VideoRecord {
        category: Some(category.to_lowercase()),
        source_url: Some(body.url.clone()),
        ..Default::default()
    };

    match ingest_from_url(seed, &selection, &state).await {
        Ok(record) => {
            let status = playback::status(&record, &state.config);
            HttpResponse::Created().json(VideoDescriptor {
                original_file_url: record
                    .original_s3_path
                    .as_deref()
                    .or(record.downloaded_s3_path.as_deref())
                    .and_then(|p| state.cf.get_signed_url(p).ok()),
                playback: status,
                record,
            })
        }
        Err(e) => {
            error!("Error ingesting {}: {:#}", body.url, e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

/// POST /api/video/:category — accept a multipart upload.
async fn upload_video(
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    mut payload: Multipart,
    state: web::Data<AppState>,
) -> HttpResponse {
    let category = path.into_inner();

    let default_immediate = state.config.transcode.processing.trim() != "deferred";
    let selection = match selection_from_query(
        query.get("renditions"),
        query.get("defer"),
        default_immediate,
        &state,
    ) {
        Ok(s) => s,
        Err(msg) => {
            error!("Upload rejected for category={}: {}", category, msg);
            return HttpResponse::BadRequest().body(msg);
        }
    };

    let mut found_file: Option<(tempfile::NamedTempFile, String, String)> = None;

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(f) => f,
            Err(e) => {
                error!("Multipart error: {}", e);
                return HttpResponse::BadRequest().body("Multipart error");
            }
        };

        let field_name = field
            .content_disposition()
            .and_then(|cd| cd.get_name())
            .unwrap_or("")
            .to_string();

        if !UPLOAD_FIELDS.contains(&field_name.as_str()) {
            continue;
        }

        let original_filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename())
            .unwrap_or("")
            .to_string();

        let content_type = field
            .content_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let ext = extension_from_path(&original_filename);

        let tmp = match TempBuilder::new().suffix(&ext).tempfile() {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to create temp file: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        };

        let mut written = tmp;
        while let Some(chunk) = field.next().await {
            let data = match chunk {
                Ok(d) => d,
                Err(e) => {
                    error!("Error reading multipart chunk: {}", e);
                    return HttpResponse::BadRequest().body("Error reading upload");
                }
            };
            if let Err(e) = written.write_all(&data) {
                error!("Error writing temp file: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
        }

        found_file = Some((written, content_type, ext));
        break;
    }

    let Some((tmp_file, content_type, ext)) = found_file else {
        error!("No video file supplied");
        return HttpResponse::BadRequest()
            .body(format!("No video file supplied (expected a field named {UPLOAD_FIELDS:?})"));
    };

    let tmp_path = tmp_file.path().to_path_buf();
    debug!("Video uploaded: path={:?} mime={} selection={:?}", tmp_path, content_type, selection);

    match process_video(&tmp_path, &ext, &content_type, &category, None, &selection, None, &state).await
    {
        Ok(record) => {
            let status = playback::status(&record, &state.config);
            if status.state != "ready" {
                // Worth a line in the log: an upload that is not immediately
                // playable is either an unusual source or a misconfiguration, and
                // the probe result is what tells them apart.
                warn!(
                    "upload {:?} is not immediately playable ({:?}); probe={:?}",
                    record.id, status.reason, record.probe
                );
            }
            // tmp_file drops here, auto-deleting the temp file.
            HttpResponse::Created().json(VideoDescriptor {
                original_file_url: record
                    .original_s3_path
                    .as_deref()
                    .or(record.downloaded_s3_path.as_deref())
                    .and_then(|p| state.cf.get_signed_url(p).ok()),
                playback: status,
                record,
            })
        }
        Err(e) => {
            error!("Error processing uploaded video: {}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}
