//! Finding out what we were actually given.
//!
//! This is the step the image service has no equivalent of, and everything about
//! the fallback behaviour depends on it. A JPEG is renderable by inspection; an
//! uploaded video may be ProRes in a MOV, VP9 in a MKV, or — the case that causes
//! the most confusion — perfectly ordinary H.264/AAC that still will not start,
//! because its `moov` index sits after the media data.
//!
//! Two independent questions get answered here:
//!
//! * **Are the codecs and container ones a browser opens?** From ffprobe.
//! * **Is the index at the front?** From reading the file's own box structure, not
//!   from ffprobe — see [`mp4_faststart`].

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::config::FfmpegConfig;
use crate::models::video::SourceProbe;

/// Video codecs a mainstream browser decodes.
///
/// HEVC is deliberately absent. Safari plays it and Chrome will on some hardware,
/// which makes it exactly the wrong thing to gamble a fallback on: the viewer who
/// cannot decode it sees a dead player rather than a spinner, and has no way to tell
/// that waiting would have helped.
const PLAYABLE_VIDEO: [&str; 4] = ["h264", "vp8", "vp9", "av1"];

/// Audio codecs a mainstream browser decodes. A file with no audio track at all is
/// also fine — silence plays.
const PLAYABLE_AUDIO: [&str; 5] = ["aac", "mp3", "opus", "vorbis", "flac"];

/// Containers a browser will open directly.
///
/// `mov` is not among them. Its codecs are frequently fine and browsers often cope,
/// but not reliably across Firefox, and a stream-copy remux into MP4 costs nothing
/// and removes the question — so a MOV is treated as something to remux, not
/// something to serve.
const PLAYABLE_CONTAINER: [&str; 3] = ["mp4", "m4v", "webm"];

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    format: FfprobeFormat,
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Debug, Deserialize, Default)]
struct FfprobeFormat {
    #[serde(default)]
    format_name: String,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
}

pub struct FfprobeService {
    bin: String,
    timeout_seconds: u32,
}

impl FfprobeService {
    pub fn new(cfg: &FfmpegConfig) -> Self {
        Self {
            bin: cfg.probe_bin.clone(),
            timeout_seconds: cfg.timeout_seconds,
        }
    }

    /// Probe a local file.
    ///
    /// Returns `Err` only when ffprobe could not be run or its output could not be
    /// understood. Callers treat that as "unknown", not as "bad file": an upload
    /// still gets stored, and the playback chain falls back to the file extension
    /// with `verified: false`. Refusing the upload because the probe failed would
    /// turn a missing binary into data loss.
    pub async fn probe(&self, path: &Path) -> Result<SourceProbe> {
        let args = [
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ];

        debug!("ffprobe: {} {:?} {:?}", self.bin, args, path);
        let output = timeout(
            Duration::from_secs(self.timeout_seconds.max(1) as u64),
            Command::new(&self.bin)
                .args(args)
                .arg(path)
                .output(),
        )
        .await
        .with_context(|| format!("ffprobe timed out after {}s", self.timeout_seconds))?
        .with_context(|| format!("Failed to execute ffprobe binary: {}", self.bin))?;

        if !output.status.success() {
            anyhow::bail!(
                "ffprobe failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)
            .context("Failed to parse ffprobe JSON output")?;

        let video = parsed.streams.iter().find(|s| s.codec_type == "video");
        let audio = parsed.streams.iter().find(|s| s.codec_type == "audio");

        let container = container_name(&parsed.format, path);
        let video_codec = video.map(|s| s.codec_name.clone()).unwrap_or_default();
        let audio_codec = audio.map(|s| s.codec_name.clone());

        let codecs_playable = PLAYABLE_VIDEO.contains(&video_codec.as_str())
            // A file with no audio track at all is fine — silence plays.
            && audio_codec
                .as_deref()
                .is_none_or(|a| PLAYABLE_AUDIO.contains(&a));
        let container_playable = PLAYABLE_CONTAINER.contains(&container.as_str());

        let faststart = is_faststart(path, &container).await;

        let probe = SourceProbe {
            container,
            video_codec,
            audio_codec,
            duration_seconds: parsed.format.duration.and_then(|d| d.parse().ok()),
            width: video.and_then(|s| s.width),
            height: video.and_then(|s| s.height),
            codecs_playable,
            browser_playable: codecs_playable && container_playable,
            faststart,
        };
        debug!("ffprobe result for {:?}: {:?}", path, probe);
        Ok(probe)
    }
}

/// Normalise ffprobe's `format_name` into one word.
///
/// ffprobe reports the whole ISOBMFF family as a single demuxer —
/// `mov,mp4,m4a,3gp,3g2,mj2` — so it cannot on its own tell an MP4 from a
/// QuickTime MOV. The `major_brand` tag can (`qt  ` means MOV), and the file
/// extension is the last resort.
fn container_name(format: &FfprobeFormat, path: &Path) -> String {
    let names: Vec<&str> = format.format_name.split(',').map(str::trim).collect();
    let isobmff = names.contains(&"mp4") || names.contains(&"mov");

    if isobmff {
        if let Some(brand) = format.tags.get("major_brand") {
            if brand.trim().eq_ignore_ascii_case("qt") {
                return "mov".to_string();
            }
            return "mp4".to_string();
        }
        return match extension_of(path).as_str() {
            "mov" => "mov".to_string(),
            "m4v" => "m4v".to_string(),
            _ => "mp4".to_string(),
        };
    }

    names.first().copied().unwrap_or_default().to_string()
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Is this file's index positioned for progressive playback?
async fn is_faststart(path: &Path, container: &str) -> bool {
    match container {
        // WebM/Matroska carries its cues wherever the muxer put them, but browsers
        // stream it without needing the whole file first, so the question does not
        // arise in the form it does for MP4.
        "webm" | "matroska" => true,
        "mp4" | "m4v" | "mov" => match mp4_faststart(path).await {
            Ok(v) => v,
            Err(e) => {
                // Unreadable box structure is not proof of anything. Say no, because
                // the cost of being wrong that way is a remux nobody needed, and the
                // cost of being wrong the other way is a player that hangs.
                warn!("could not read MP4 box order for {:?}: {e:#}", path);
                false
            }
        },
        _ => false,
    }
}

/// Walk an MP4's top-level boxes and report whether `moov` precedes `mdat`.
///
/// Done here rather than by shelling out because it is both cheaper and more
/// certain: ffprobe will happily open a tail-indexed file without ever mentioning
/// that it had to seek to the end to do so. The structure is a flat sequence of
/// `[u32 size][4-byte type]` headers, with two escapes — size 1 means a 64-bit
/// length follows the type, and size 0 means the box runs to end of file — so
/// finding which of the two boxes comes first is a handful of seeks.
pub(crate) async fn mp4_faststart(path: &Path) -> Result<bool> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("Failed to open {:?}", path))?;
    let len = file.metadata().await?.len();

    let mut offset: u64 = 0;
    let mut header = [0u8; 16];

    while offset + 8 <= len {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let want = if offset + 16 <= len { 16 } else { 8 };
        file.read_exact(&mut header[..want]).await?;

        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let box_type = &header[4..8];

        match box_type {
            b"moov" => return Ok(true),
            b"mdat" => return Ok(false),
            _ => {}
        }

        let box_size = match size32 {
            // Size 0: this box extends to EOF, so nothing follows it. Neither moov
            // nor mdat has been seen, which is a file we cannot judge.
            0 => break,
            // Size 1: the real, 64-bit size sits in the eight bytes after the type.
            1 => {
                if want < 16 {
                    break;
                }
                u64::from_be_bytes([
                    header[8], header[9], header[10], header[11], header[12], header[13],
                    header[14], header[15],
                ])
            }
            n => n,
        };

        // A box that claims to be smaller than its own header would loop forever.
        if box_size < 8 {
            anyhow::bail!("malformed MP4 box at offset {offset}: size {box_size}");
        }
        offset += box_size;
    }

    anyhow::bail!("no moov or mdat box found in {:?}", path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn format(name: &str, brand: Option<&str>) -> FfprobeFormat {
        let mut tags = HashMap::new();
        if let Some(b) = brand {
            tags.insert("major_brand".to_string(), b.to_string());
        }
        FfprobeFormat {
            format_name: name.to_string(),
            duration: None,
            tags,
        }
    }

    /// ffprobe reports one demuxer for the whole ISOBMFF family, so the brand is
    /// what separates a QuickTime MOV from an MP4 — and the two are treated
    /// differently, one being remuxed and the other served.
    #[test]
    fn the_major_brand_distinguishes_mov_from_mp4() {
        let f = format("mov,mp4,m4a,3gp,3g2,mj2", Some("qt  "));
        assert_eq!(container_name(&f, Path::new("x.mov")), "mov");

        let f = format("mov,mp4,m4a,3gp,3g2,mj2", Some("isom"));
        assert_eq!(container_name(&f, Path::new("x.mp4")), "mp4");
    }

    /// Without a brand tag the extension is all that is left.
    #[test]
    fn the_extension_is_the_fallback_witness() {
        let f = format("mov,mp4,m4a,3gp,3g2,mj2", None);
        assert_eq!(container_name(&f, Path::new("clip.mov")), "mov");
        assert_eq!(container_name(&f, Path::new("clip.mp4")), "mp4");
        assert_eq!(container_name(&f, Path::new("clip")), "mp4");
    }

    #[test]
    fn a_non_isobmff_container_keeps_its_own_name() {
        assert_eq!(container_name(&format("matroska,webm", None), Path::new("x.webm")), "matroska");
    }

    /// Build a minimal MP4 box sequence and check which of moov/mdat is found first.
    /// Only the box headers matter, so the payloads are zeroes.
    fn mp4_with(order: &[&[u8; 4]]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".mp4").tempfile().unwrap();
        // A leading ftyp, as every real file has, to prove the walk skips boxes it
        // does not care about rather than assuming the first one is the answer.
        let mut types: Vec<&[u8]> = vec![b"ftyp"];
        types.extend(order.iter().map(|t| t.as_slice()));

        for (i, ty) in types.into_iter().enumerate() {
            let payload = 8 + i; // arbitrary, non-zero, and different per box
            let size = (8 + payload) as u32;
            f.write_all(&size.to_be_bytes()).unwrap();
            f.write_all(ty).unwrap();
            f.write_all(&vec![0u8; payload]).unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn a_moov_before_mdat_is_faststart() {
        let f = mp4_with(&[b"moov", b"mdat"]);
        assert!(mp4_faststart(f.path()).await.unwrap());
    }

    /// The case this whole module exists for: entirely valid, entirely undecodable
    /// to start with, and indistinguishable from a broken file to the viewer.
    #[tokio::test]
    async fn a_mdat_before_moov_is_not_faststart() {
        let f = mp4_with(&[b"mdat", b"moov"]);
        assert!(!mp4_faststart(f.path()).await.unwrap());
    }

    /// A box claiming an impossible size must abort rather than spin.
    #[tokio::test]
    async fn a_malformed_box_size_is_an_error_not_a_hang() {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".mp4").tempfile().unwrap();
        f.write_all(&3u32.to_be_bytes()).unwrap();
        f.write_all(b"junk").unwrap();
        f.write_all(&[0u8; 32]).unwrap();
        f.flush().unwrap();
        assert!(mp4_faststart(f.path()).await.is_err());
    }

    /// WebM is streamable in the way that matters here, so it must not be sent for a
    /// pointless remux.
    #[tokio::test]
    async fn webm_is_treated_as_progressive() {
        assert!(is_faststart(Path::new("/nonexistent.webm"), "webm").await);
        // And an unknown container is not.
        assert!(!is_faststart(Path::new("/nonexistent.avi"), "avi").await);
    }

    /// Hand-built boxes prove the walk; only a real muxer proves the premise.
    ///
    /// This is the claim the whole fallback rests on — that an ordinary MP4 comes
    /// out tail-indexed unless something asks otherwise — so it is worth checking
    /// against ffmpeg rather than against our own idea of the format. Skipped when
    /// ffmpeg is absent, so it never fails a build for the wrong reason.
    #[tokio::test]
    async fn real_ffmpeg_output_matches_the_faststart_detector() {
        let Some(ffmpeg) = which("ffmpeg") else {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        };
        let dir = tempfile::tempdir().unwrap();

        // Two files from the same source, differing only in the flag under test.
        for (name, faststart, expected) in
            [("tail.mp4", false, false), ("front.mp4", true, true)]
        {
            let out = dir.path().join(name);
            let mut cmd = tokio::process::Command::new(&ffmpeg);
            cmd.args(["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=15:duration=1"])
                .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
            if faststart {
                cmd.args(["-movflags", "+faststart"]);
            }
            let status = cmd.arg(&out).output().await.unwrap();
            assert!(status.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&status.stderr));

            assert_eq!(
                mp4_faststart(&out).await.unwrap(),
                expected,
                "{name} (faststart flag: {faststart})"
            );
        }
    }

    /// The remuxable case, end to end against the real binaries: a MOV that a
    /// browser will not open, whose codecs are perfectly fine.
    #[tokio::test]
    async fn a_real_mov_probes_as_remuxable_not_servable() {
        let (Some(ffmpeg), Some(ffprobe)) = (which("ffmpeg"), which("ffprobe")) else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.mov");

        let made = tokio::process::Command::new(&ffmpeg)
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=15:duration=1"])
            .args(["-f", "lavfi", "-i", "anullsrc=channel_layout=stereo:sample_rate=48000"])
            .args(["-shortest", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&out)
            .output()
            .await
            .unwrap();
        assert!(made.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&made.stderr));

        let svc = FfprobeService {
            bin: ffprobe.to_string_lossy().into_owned(),
            timeout_seconds: 60,
        };
        let probe = svc.probe(&out).await.unwrap();

        assert_eq!(probe.container, "mov", "brand should identify QuickTime");
        assert_eq!(probe.video_codec, "h264");
        assert_eq!(probe.audio_codec.as_deref(), Some("aac"));
        assert!(probe.codecs_playable, "h264/aac are fine");
        assert!(!probe.browser_playable, "the MOV container is not");
        assert!(!probe.directly_servable(), "so it must not be served as-is");
        assert!(probe.remuxable(), "but a stream copy fixes it");
        assert_eq!(probe.width, Some(160));
        assert_eq!(probe.height, Some(120));
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(bin))
                .find(|p| p.is_file())
        })
    }
}
