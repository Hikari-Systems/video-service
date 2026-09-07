//! The one piece of local video work this service does.
//!
//! Transcoding belongs to MediaConvert — it is minutes of CPU per input, and on a
//! spot fleet a node that vanishes mid-job takes the work with it. A **remux** is a
//! different animal: it copies the encoded frames through untouched and rewrites
//! only the container and the index. No decode, no encode, no quality loss, and it
//! finishes in seconds on a file that would take minutes to re-encode.
//!
//! That is enough to fix the two things that stop an otherwise-fine upload from
//! playing — the index at the tail of the file, and a QuickTime wrapper around
//! perfectly ordinary H.264 — so it is worth doing inside the upload request, where
//! it turns "wait for the queue" into "plays immediately".

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::Builder as TempBuilder;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::FfmpegConfig;

pub struct FfmpegService {
    bin: String,
    timeout_seconds: u32,
}

impl FfmpegService {
    pub fn new(cfg: &FfmpegConfig) -> Self {
        Self {
            bin: cfg.bin.clone(),
            timeout_seconds: cfg.timeout_seconds,
        }
    }

    /// Stream-copy `source` into a faststart MP4.
    ///
    /// Returns the path of an output temp file the caller owns and must delete.
    pub async fn remux_faststart(&self, source: &Path) -> Result<PathBuf> {
        let tmp = TempBuilder::new()
            .suffix(".mp4")
            .tempfile()
            .context("Failed to create temp file for remux output")?;
        let (_, dest) = tmp.keep().context("Failed to persist remux temp file")?;

        let args = remux_args(source, &dest);
        debug!("ffmpeg remux: {} {:?}", self.bin, args);

        let output = timeout(
            Duration::from_secs(self.timeout_seconds.max(1) as u64),
            Command::new(&self.bin).args(&args).output(),
        )
        .await
        .with_context(|| {
            format!(
                "ffmpeg remux timed out after {}s for {:?}",
                self.timeout_seconds, source
            )
        })?
        .with_context(|| format!("Failed to execute ffmpeg binary: {}", self.bin))?;

        if !output.status.success() {
            let _ = tokio::fs::remove_file(&dest).await;
            anyhow::bail!(
                "ffmpeg remux failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        info!("ffmpeg remux done: {:?} → {:?}", source, dest);
        Ok(dest)
    }
}

/// The remux command line, factored out so it can be asserted on.
///
/// Every flag is load-bearing:
///
/// * `-c copy` — the whole point. Frames are moved, not re-encoded.
/// * `-movflags +faststart` — rewrites the file with `moov` ahead of `mdat`, which
///   is what lets a player start on the first bytes rather than pulling the lot.
/// * `-map 0:v:0 -map 0:a:0?` — take the first video track and the first audio track
///   *if there is one*. The `?` matters: without it a silent clip fails outright.
///   Explicit maps also drop timed-metadata and subtitle tracks that MP4 will not
///   hold, which would otherwise fail the mux on files from some cameras.
/// * `-y` — the destination temp file already exists, so ffmpeg must be willing to
///   overwrite it instead of stopping to ask.
fn remux_args(source: &Path, dest: &Path) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-i".to_string(),
        source.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "0:a:0?".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        dest.to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remux_never_re_encodes() {
        let args = remux_args(Path::new("/in.mov"), Path::new("/out.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"), "{joined}");
        assert!(joined.contains("-movflags +faststart"), "{joined}");
        // A quality flag here would mean someone had turned a byte copy into a
        // transcode, which is exactly the cost this avoids.
        for forbidden in ["-crf", "-b:v", "libx264", "-preset"] {
            assert!(!joined.contains(forbidden), "remux must not encode: {forbidden}");
        }
    }

    /// A clip with no audio track is ordinary — screen recordings, drone footage —
    /// and an unconditional audio map would fail every one of them.
    #[test]
    fn the_audio_map_is_optional() {
        let args = remux_args(Path::new("/in.mov"), Path::new("/out.mp4"));
        assert!(args.iter().any(|a| a == "0:a:0?"), "audio map must be optional");
    }

    /// The claim the fallback chain rests on, checked against the real binary: a
    /// file ffmpeg produced tail-indexed comes back progressive, and the frames are
    /// untouched on the way through.
    #[tokio::test]
    async fn a_real_remux_turns_a_tail_indexed_file_into_a_playable_one() {
        let Some(ffmpeg) = which("ffmpeg") else {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("tail.mov");

        // A QuickTime MOV with its index at the tail — the shape a phone or camera
        // hands over, and the one that looks like a hang rather than an error.
        let made = tokio::process::Command::new(&ffmpeg)
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=15:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&source)
            .output()
            .await
            .unwrap();
        assert!(made.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&made.stderr));
        assert!(
            !crate::services::ffprobe::mp4_faststart(&source).await.unwrap(),
            "premise: the source must start out tail-indexed"
        );

        let svc = FfmpegService {
            bin: ffmpeg.to_string_lossy().into_owned(),
            timeout_seconds: 120,
        };
        let out = svc.remux_faststart(&source).await.unwrap();

        assert!(
            crate::services::ffprobe::mp4_faststart(&out).await.unwrap(),
            "the remux must move the index to the front"
        );

        // A stream copy, so the encoded payload is essentially the same size. A
        // re-encode at these settings would move it far more than this.
        let src_len = std::fs::metadata(&source).unwrap().len() as f64;
        let out_len = std::fs::metadata(&out).unwrap().len() as f64;
        assert!(
            (out_len / src_len - 1.0).abs() < 0.10,
            "expected a byte copy, got {src_len} -> {out_len}"
        );

        std::fs::remove_file(&out).unwrap();
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(bin))
                .find(|p| p.is_file())
        })
    }

    #[test]
    fn the_destination_is_last_and_overwriting_is_allowed() {
        let args = remux_args(Path::new("/in.mov"), Path::new("/out.mp4"));
        assert_eq!(args.last().unwrap(), "/out.mp4");
        assert_eq!(args.first().unwrap(), "-y");
    }
}
