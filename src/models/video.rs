use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One produced output.
///
/// `kind` decides how it is delivered: `progressive` is a single MP4 behind one
/// signed URL; `hls` is a manifest whose segments sit beside it under a shared
/// prefix, and needs signed cookies rather than a signed URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rendition {
    pub name: String,
    #[serde(rename = "s3Path")]
    pub s3_path: String,
    #[serde(rename = "mimeType", default = "default_mime")]
    pub mime_type: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

fn default_mime() -> String { "video/mp4".to_string() }
fn default_kind() -> String { "progressive".to_string() }

impl Rendition {
    pub fn is_hls(&self) -> bool {
        self.kind.eq_ignore_ascii_case("hls")
    }
}

/// What ffprobe found in the uploaded file.
///
/// This exists to answer one question the image service never has to ask: *can a
/// browser play the bytes we were given, right now, before any transcode?* For an
/// image the answer is always yes, which is why its fallback chain can redirect
/// blindly. For video it is often no, and the two ways it is no are independent —
/// see [`Self::browser_playable`] and [`Self::faststart`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SourceProbe {
    #[serde(default)]
    pub container: String,
    #[serde(rename = "videoCodec", default)]
    pub video_codec: String,
    #[serde(rename = "audioCodec", skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(rename = "durationSeconds", skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    /// The codecs alone are ones a mainstream browser decodes, regardless of what
    /// they are wrapped in.
    ///
    /// Separate from [`Self::browser_playable`] because it answers a different
    /// question: whether a **stream copy** can rescue this file. H.264/AAC inside a
    /// QuickTime MOV is not directly servable, but remuxing it into an MP4 is a byte
    /// copy — no re-encode, no quality loss, seconds rather than minutes. False here
    /// means only MediaConvert can help.
    #[serde(rename = "codecsPlayable", default)]
    pub codecs_playable: bool,
    /// Container *and* codecs a mainstream browser decodes without help.
    ///
    /// False for ProRes, MKV, AVI and friends — for those there is no fallback to
    /// offer and the viewer has to wait for MediaConvert.
    #[serde(rename = "browserPlayable", default)]
    pub browser_playable: bool,
    /// The `moov` index sits ahead of the media data, so a player can start on the
    /// first bytes instead of fetching to the end of the file first.
    ///
    /// Independent of [`Self::browser_playable`]: most phone and camera MP4s are
    /// perfectly decodable *and* have the index at the tail, which is precisely the
    /// case that looks like a hang rather than an error. [`crate::services::ffmpeg`]
    /// fixes it with a stream copy.
    #[serde(default)]
    pub faststart: bool,
}

impl SourceProbe {
    /// Can the raw upload be served to a player exactly as it arrived?
    ///
    /// Both halves are required. Decodable-but-tail-indexed is the trap: it plays
    /// eventually, after the player has pulled the whole file, which on anything
    /// large is indistinguishable from broken.
    pub fn directly_servable(&self) -> bool {
        self.browser_playable && self.faststart
    }

    /// Can a stream copy make this file directly servable?
    ///
    /// True whenever the codecs are fine and something else is not — the wrong
    /// container, the index in the wrong place, or both. This is the cheap fix that
    /// closes most of the gap between "uploaded" and "watchable" without waiting on
    /// a transcode, and it is worth doing exactly when the file is not already
    /// servable.
    pub fn remuxable(&self) -> bool {
        self.codecs_playable && !self.directly_servable()
    }
}

/// Status of one submitted MediaConvert job.
///
/// A record can hold more than one: [`crate::config::TranscodeConfig::proxy_rendition`]
/// splits a submission into a fast proxy job and a slower ladder job, because
/// MediaConvert releases a job's outputs together and a single job would make the
/// cheap rendition wait for the expensive one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscodeJob {
    /// MediaConvert job id.
    pub id: String,
    /// `SUBMITTED` | `PROGRESSING` | `COMPLETE` | `ERROR` | `CANCELED`.
    pub status: String,
    /// Which renditions this job was asked to produce, so a completion can be
    /// written back without guessing from output filenames.
    pub renditions: Vec<String>,
    #[serde(rename = "submittedAt")]
    pub submitted_at: DateTime<Utc>,
    #[serde(rename = "percentComplete", skip_serializing_if = "Option::is_none")]
    pub percent_complete: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TranscodeJob {
    pub fn is_in_flight(&self) -> bool {
        matches!(self.status.as_str(), "SUBMITTED" | "PROGRESSING")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoRecord {
    pub id: Option<Uuid>,
    pub category: Option<String>,
    #[serde(rename = "sourceUrl", skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// The raw upload, byte for byte. Always written; not always playable.
    #[serde(rename = "downloadedS3Path", skip_serializing_if = "Option::is_none")]
    pub downloaded_s3_path: Option<String>,
    /// A **playable** copy of the source — either the upload itself when it was
    /// already progressive, or its faststart remux. Only ever set when the bytes
    /// behind it will actually play, which is what lets the fallback chain redirect
    /// to it without checking anything further.
    #[serde(rename = "originalS3Path", skip_serializing_if = "Option::is_none")]
    pub original_s3_path: Option<String>,
    #[serde(rename = "renditionFiles", skip_serializing_if = "Option::is_none")]
    pub rendition_files: Option<Vec<Rendition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<SourceProbe>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<TranscodeJob>,
    #[serde(rename = "avoidTranscodeUntil", skip_serializing_if = "Option::is_none")]
    pub avoid_transcode_until: Option<DateTime<Utc>>,
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

impl VideoRecord {
    pub fn renditions(&self) -> &[Rendition] {
        self.rendition_files.as_deref().unwrap_or(&[])
    }

    pub fn rendition(&self, name: &str) -> Option<&Rendition> {
        self.renditions().iter().find(|r| r.name == name)
    }

    pub fn in_flight_jobs(&self) -> impl Iterator<Item = &TranscodeJob> {
        self.jobs.iter().filter(|j| j.is_in_flight())
    }

    pub fn has_in_flight_job(&self) -> bool {
        self.in_flight_jobs().next().is_some()
    }

    /// Merge one produced rendition in, replacing any entry with the same name.
    ///
    /// Merge rather than replace, for the same reason the image service merges: a
    /// proxy job and a ladder job complete minutes apart and each writes only what
    /// it produced. A wholesale write would have the second one erase the first.
    pub fn merge_rendition(&mut self, r: Rendition) {
        let files = self.rendition_files.get_or_insert_with(Vec::new);
        match files.iter_mut().find(|e| e.name == r.name) {
            Some(existing) => *existing = r,
            None => files.push(r),
        }
    }

    /// Record the current state of a job, replacing any earlier state for the same id.
    pub fn merge_job(&mut self, job: TranscodeJob) {
        match self.jobs.iter_mut().find(|j| j.id == job.id) {
            Some(existing) => *existing = job,
            None => self.jobs.push(job),
        }
    }
}

/// The video descriptor returned to callers: the stored record plus a signed URL
/// for the source, and the playback status the client needs in order to decide
/// between playing something and showing a spinner.
#[derive(Debug, Clone, Serialize)]
pub struct VideoDescriptor {
    #[serde(flatten)]
    pub record: VideoRecord,
    #[serde(rename = "originalFileUrl", skip_serializing_if = "Option::is_none")]
    pub original_file_url: Option<String>,
    pub playback: crate::helpers::playback::PlaybackStatus,
}

/// One category's expected rendition count, for the sweep's incompleteness test.
pub struct ExpectedRenditions {
    /// Lower-cased category name — categories are lower-cased before their
    /// rendition set is looked up, so the comparison must be too.
    pub category: String,
    pub count: i32,
}

#[async_trait]
pub trait VideoBackend: Send + Sync {
    async fn get(&self, id: &str) -> Result<Option<VideoRecord>>;
    async fn upsert(&self, video: VideoRecord) -> Result<VideoRecord>;

    /// Atomically take ownership of up to `limit` videos that still need a job
    /// submitted, holding each for `lease_seconds`.
    ///
    /// The contract is **exclusivity**: two nodes running this concurrently must
    /// never receive the same video, or the account is billed twice for one
    /// transcode. Backends that cannot guarantee it return an empty vec — a claim
    /// that is merely *probably* exclusive is worse than none, because it looks
    /// like it works.
    async fn claim_for_submit(
        &self,
        limit: i64,
        lease_seconds: i64,
        expected: &[ExpectedRenditions],
        default_expected: i32,
    ) -> Result<Vec<VideoRecord>> {
        let _ = (limit, lease_seconds, expected, default_expected);
        Ok(Vec::new())
    }

    /// Atomically take ownership of up to `limit` videos with an in-flight job, so
    /// their status can be polled and completions written back.
    async fn claim_for_reconcile(
        &self,
        limit: i64,
        lease_seconds: i64,
    ) -> Result<Vec<VideoRecord>> {
        let _ = (limit, lease_seconds);
        Ok(Vec::new())
    }

    /// Set (or clear) a video's transcode lease without touching anything else.
    ///
    /// Deliberately not `upsert`: the sweep must not write back a whole record it
    /// read seconds ago, because a reconcile may have updated that row in between
    /// and a full write would stamp the stale copy over it.
    async fn set_transcode_lease(&self, id: Uuid, until: Option<DateTime<Utc>>) -> Result<()> {
        let _ = (id, until);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendition(name: &str, path: &str) -> Rendition {
        Rendition {
            name: name.into(),
            s3_path: path.into(),
            mime_type: "video/mp4".into(),
            kind: "progressive".into(),
            width: None,
            height: None,
        }
    }

    /// The proxy job and the ladder job finish minutes apart and each writes only
    /// what it produced. If the second write replaced rather than merged, the fast
    /// rendition the viewer is already watching would vanish.
    #[test]
    fn merging_a_rendition_keeps_what_an_earlier_job_produced() {
        let mut rec = VideoRecord::default();
        rec.merge_rendition(rendition("proxy", "clip-1-proxy.mp4"));
        rec.merge_rendition(rendition("hd", "clip-1-hd.mp4"));
        assert_eq!(rec.renditions().len(), 2);
        assert!(rec.rendition("proxy").is_some());

        // A re-run of the proxy replaces it in place rather than duplicating it.
        rec.merge_rendition(rendition("proxy", "clip-1-proxy-v2.mp4"));
        assert_eq!(rec.renditions().len(), 2);
        assert_eq!(rec.rendition("proxy").unwrap().s3_path, "clip-1-proxy-v2.mp4");
    }

    /// Decodable but tail-indexed is the trap this whole probe exists for: it is
    /// not an error, it just never starts. It must not count as servable.
    #[test]
    fn a_tail_indexed_mp4_is_not_directly_servable() {
        let probe = SourceProbe {
            browser_playable: true,
            faststart: false,
            ..Default::default()
        };
        assert!(!probe.directly_servable());

        let fixed = SourceProbe { faststart: true, ..probe };
        assert!(fixed.directly_servable());
    }

    /// A ProRes .mov is faststart often enough, and still unplayable everywhere.
    #[test]
    fn faststart_alone_does_not_make_a_source_servable() {
        let probe = SourceProbe {
            browser_playable: false,
            faststart: true,
            ..Default::default()
        };
        assert!(!probe.directly_servable());
    }

    #[test]
    fn only_submitted_and_progressing_jobs_are_in_flight() {
        let job = |status: &str| TranscodeJob {
            id: status.into(),
            status: status.into(),
            renditions: vec![],
            submitted_at: Utc::now(),
            percent_complete: None,
            error: None,
        };
        let mut rec = VideoRecord::default();
        for s in ["COMPLETE", "ERROR", "CANCELED"] {
            rec.merge_job(job(s));
        }
        assert!(!rec.has_in_flight_job());
        rec.merge_job(job("PROGRESSING"));
        assert!(rec.has_in_flight_job());
    }

    /// Polling updates a job in place; appending would grow the array without bound
    /// and leave a stale PROGRESSING entry making the record look permanently busy.
    #[test]
    fn merging_a_job_updates_it_in_place() {
        let mut rec = VideoRecord::default();
        let base = TranscodeJob {
            id: "job-1".into(),
            status: "SUBMITTED".into(),
            renditions: vec!["proxy".into()],
            submitted_at: Utc::now(),
            percent_complete: None,
            error: None,
        };
        rec.merge_job(base.clone());
        rec.merge_job(TranscodeJob { status: "COMPLETE".into(), ..base });
        assert_eq!(rec.jobs.len(), 1);
        assert_eq!(rec.jobs[0].status, "COMPLETE");
        assert!(!rec.has_in_flight_job());
    }
}
