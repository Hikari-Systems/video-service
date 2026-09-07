# CLAUDE.md — video-service-rs

Guidance for AI assistants working on this codebase.

---

## What this service does

Accepts video uploads, stores them in S3, transcodes them to multiple renditions via
**AWS Elemental MediaConvert**, and serves signed CloudFront URLs (and, for HLS,
signed cookies) for delivery. Metadata lives in PostgreSQL or on the filesystem.

It is deliberately built to the same shape as `image-service-rs`: same config
layering, same `AppState`/backend split, same route conventions, same Docker and CI
setup. **Read that service first** — most of the structure here is its structure, and
the interesting parts of this repo are precisely the places where video forced a
difference.

---

## Codebase map

```
src/
  main.rs                  — startup: config, backend, migrations, sweep, server bind
  config.rs                — AppConfig + sub-structs; 3-layer loader (no `config` crate)
  state.rs                 — AppState (concrete struct; only the backend is dyn)
  routes/
    video.rs               — upload, url ingest, get, redirect, signed URL, status, cookies, transcode
    category.rs            — GET /api/category/list
  models/
    video.rs               — VideoRecord, Rendition, SourceProbe, TranscodeJob, VideoBackend trait
    video_db.rs            — PostgreSQL backend; both exclusive claim queries
    video_file.rs          — filesystem backend; deliberately cannot claim
  services/
    s3.rs                  — S3Service: single-shot under 64 MiB, multipart above
    cloudfront.rs          — signed URLs (canned) and signed cookies (custom policy)
    ffprobe.rs             — the probe, and the MP4 box walk that finds the index
    ffmpeg.rs              — the faststart remux. The ONLY local video work.
    mediaconvert.rs        — job building, submission, polling
    downloader.rs          — streaming fetch for URL ingest
    sweeper.rs             — reconcile + submit passes
  helpers/
    playback.rs            — the CHECKED fallback chain. Start here.
    transcode.rs           — selection parsing, upload pipeline, job orchestration
```

---

## The three things that are not like image-service

Everything unusual in this repo traces back to one of these. If a change seems to be
fighting the design, check which one it is fighting.

### 1. Transcoding is remote and asynchronous

`transcode_image` returns when the file exists. `submit_jobs` returns when AWS has
*accepted* a job. Nothing in this service waits for a transcode.

Consequence: **`services/sweeper.rs` is load-bearing, not a convenience.** With the
sweep off, jobs are submitted and their outputs never recorded. `main.rs` warns at
startup when MediaConvert is configured and the sweep is not.

### 2. An upload is not necessarily playable

The image service's fallback chain redirects blindly because every rung renders.
Here, two independent things can stop an upload playing, and they need different
answers:

* **Codecs/container the browser cannot decode** (ProRes, MKV, AVI, HEVC outside
  Safari) — nothing local helps.
* **`moov` index at the tail of the file** — the default for most phone and camera
  muxers. Entirely decodable, and it will not start until the player has pulled the
  whole file. Looks like a hang, not an error.

`SourceProbe` splits these into `codecsPlayable` (can a stream copy rescue it?) and
`browserPlayable` (container too), with `faststart` separate again.
`directly_servable()` and `remuxable()` are the two questions callers actually ask.

### 3. A job's outputs are released together

MediaConvert does not release outputs incrementally. "Small one first" is a **second
job**, not an early output — hence `transcode.proxyRendition` and the two-job split
in `submit_jobs`.

---

## Critical implementation decisions

### The fallback chain is checked, never blind (`helpers/playback.rs`)
`resolve()` returns `Result<Playable, NotReady>`. Every rung it returns is something
established to play — MediaConvert produced it, or the probe cleared it. When nothing
qualifies, routes return **202 with a status body**, not a redirect.

**Never add a rung that has not been checked.** Handing a client a URL to an
unplayable file is strictly worse than telling it to wait: it cannot tell "broken"
from "not finished", so it stops polling and shows an error for something that would
have worked in ninety seconds.

### `original` is immune to the selection, unlike image-service
`RenditionSelection::wants(ORIGINAL_KEY)` returns true unless `defer` names it — it
ignores both the allowlist and `transcode.processing`.

This is not an oversight and must not be "fixed" back to symmetry. The shipped
default is `processing: "deferred"`; honouring that for the original meant every
upload landed unplayable **and stayed that way**, because nothing ever comes back for
it — the sweep submits MediaConvert jobs, it does not run remuxes. Caught by an
end-to-end test, not by a unit test, which is worth remembering.

### An already-progressive upload is not copied
`prepare_original` returns `None` when `probe.directly_servable()`. The image service
copies its source to an `-original` key in that case; here the playback chain's
rung 4 serves the raw upload directly, so a copy would duplicate every already-good
upload in S3 for no benefit. Invisible on photos, a bill on video.

The consequence to remember: `originalS3Path` being absent does **not** mean the
video is unplayable. Check `playback.ready` — it will say `source`.

### The probe runs before the S3 upload
Order matters. The stored object's content type comes from
`mime_for_container(probe.container)`, not from the multipart part, because an
uploader that sets no part type leaves `application/octet-stream` — and for a source
served directly, that is what a browser is handed and may refuse to play.

### A failed remux is not retried
If ffmpeg is missing or the remux fails, `originalS3Path` stays unset and the video
depends on MediaConvert to become playable. Deliberate: retrying would mean pulling
the source back down to a node, which is the work this design moved off the nodes in
the first place. The failure is logged with the probe result; MediaConvert still
produces renditions, so the video does become playable, just later.

### `playable_by_extension` must agree with `ffprobe::PLAYABLE_CONTAINER`
Both exclude `.mov`, and for the same reason — the codecs are usually fine but
Firefox will not reliably open the container, so a MOV is remuxed rather than served.
These are two lists that must not drift; there is a test pinning them together
(`the_extension_fallback_agrees_with_the_probe_about_mov`). This has already been a
bug once.

### The MP4 box walk is in Rust, not ffprobe (`services/ffprobe.rs`)
`mp4_faststart` reads the top-level box headers directly. ffprobe will happily open a
tail-indexed file without ever mentioning that it had to seek to the end to do it, so
it cannot answer the question. The walk handles both escapes — size 1 (64-bit length)
and size 0 (extends to EOF) — and errors rather than looping on a box smaller than
its own header.

Verified against real ffmpeg output, not just hand-built boxes: see
`real_ffmpeg_output_matches_the_faststart_detector`. Those tests skip when ffmpeg is
absent rather than failing.

### The remux never encodes (`services/ffmpeg.rs`)
`-c copy -movflags +faststart`, with `-map 0:v:0 -map 0:a:0?`. The `?` is required or
every silent clip fails. There is a test asserting no encoder flag ever appears in
that command line — if a change makes it necessary to encode locally, that is a
design change, not a flag change.

### Output keys are computed, not read back (`helpers/transcode.rs`)
`output_key()` reproduces exactly what MediaConvert's `<destination><input
basename><nameModifier><ext>` naming produces. The reconcile confirms each key with a
HEAD before recording it — a rendition recorded but absent would be a signed URL to a
404, which is the one failure the playback chain cannot detect or fall back from.

### FILE_GROUP and HLS_GROUP need different destinations
FILE_GROUP takes `s3://bucket/` and appends the input basename. HLS_GROUP takes the
literal shared prefix. `SubmitRequest` carries both. Each HLS rendition needs its
**own** group — a group emits one master manifest, so two in a group collide.

### Renditions merge, they do not replace
The proxy job and the ladder job finish minutes apart, each writing only what it
produced. `merge_rendition` / `merge_job` fold by key. A wholesale write would have
the second completion erase the rendition the viewer is already watching.

### The category lookup is case-insensitive (`config.rs`)
Categories are lower-cased everywhere in the pipeline; config keys are
case-preserving by design. A `renditionSets` entry written `shortClip` would
otherwise never match, silently fall back to the global list, and leave the sweep
judging that category against the wrong count. This has already been a bug once.

### The sweep's completeness test excludes `original`
Deliberately. A source no browser can decode legitimately has **no playable original,
ever**; requiring one would re-claim every ProRes upload forever, and each re-claim
is a duplicate MediaConvert job. Only rendition count is compared.

### The submit lease and the poll interval are different durations
`leaseSeconds` (900) guards a submission; `pollSeconds` (30) is how often an in-flight
job is polled. They were the same value once, and the result was a transcode that
finished in ~4 seconds sitting unrecorded for 15 minutes — the renditions were in S3
and the API said `pending`. Do not re-merge them.

### An upload takes the submit lease in its FIRST write
`process_video` sets `avoid_transcode_until` on the same upsert that first makes the
row visible, and `append_jobs` releases it. Skipping this leaves a window between
"row has a source" and "row has jobs" in which the sweep sees an untouched video and
submits duplicate jobs. Measured against real MediaConvert: a two-rendition upload
produced **four** jobs. Neither the unit tests nor the MinIO end-to-end run caught
it — only real AWS did.

### `submit_jobs` appends, it does not upsert
`append_jobs` is a single `jobs = jobs || $2` statement. A whole-record write there
would erase a rendition a concurrent reconcile had just recorded, and would lose one
of two concurrent submitters' job ids — leaving MediaConvert jobs running, billing,
and untracked. `POST /api/video/{id}/transcode` holds no lease, so this is a live
path, not a theoretical one.

### `claim_for_*` are the only things standing between us and double billing
Both use `UPDATE … FOR UPDATE SKIP LOCKED` with the lease written in the same
statement as the selection. The file backend does **not** override them — the trait
defaults return empty — because a claim that is merely probably exclusive is worse
than none.

The reconcile claim uses `jobs @> '[{"status":"PROGRESSING"}]'` containment rather
than a `jsonb_array_elements` subquery, specifically so the GIN index can serve it.
The subquery form is correct and unindexable.

### S3 uploads split at 64 MiB (`services/s3.rs`)
Under it, read into memory and `PutObject` — the proven image-service path, which
avoids the `ByteStream::from_path` streaming-checksum deadlock. Over it, multipart
with 16 MiB parts read sequentially. Do not "simplify" this back to a single read: a
couple of concurrent video uploads doing that would OOM the node.

### Config loading, AppState, route order, sqlx queries
Unchanged from image-service, and unchanged for the same reasons:
- **Do not use the `config` crate** — it lowercases keys and breaks camelCase.
- `AppState` is a concrete struct; only the backend is `dyn`.
- Routes register directly on `ServiceConfig`, never inside `web::scope("")`.
- Literal-segment routes register **before** `{id}`/`{category}` wildcards.
- sqlx **runtime** queries (`query_as::<_, Row>`), never the compile-time macros,
  which need `DATABASE_URL` at build time.
- `static/index.html` is embedded with `include_str!`, not served by `actix-files`.
- reqwest is `rustls-tls` only, no OpenSSL.

---

## Building and testing

There is no local Rust toolchain. Build in a container, and note that bind mounts do
not work in this environment — push source with `docker cp`:

```bash
docker run -d --name vsbuild -w /build rust:1-bookworm sleep infinity
docker cp Cargo.toml vsbuild:/build/ && docker cp src vsbuild:/build/src
docker cp config.json vsbuild:/build/ && docker cp static vsbuild:/build/static
docker cp migrations vsbuild:/build/migrations
docker exec vsbuild bash -c 'cd /build && cargo test'
```

Use `bash -c`, not `bash -lc` — the login profile drops `/usr/local/cargo/bin` from
`PATH`.

Install ffmpeg in the container (`apt-get install -y ffmpeg`) to exercise the
real-binary tests; without it they skip.

---

## Adding a rendition

1. Add an entry under `transcode` in `config.json` with `width`, `height`, `bitrate`.
2. Add its name to `transcode.renditionKeys` (or a `renditionSets` entry).
3. Optionally add it to `transcode.fallbackOrder` — omitting it is safe, it is
   appended in category order rather than becoming unreachable.

No code changes: `TranscodeConfig` captures named renditions via `#[serde(flatten)]`.

Existing videos become incomplete and the sweep picks them up on its next pass. That
is a MediaConvert job per existing video — intended, and worth being deliberate about
on a large table.

---

## Common gotchas

- Config keys are **case-sensitive**. `bucketname` is not `bucketName`.
- `original` and `transcodeSweep` are reserved. Never put them in `renditionKeys`.
- Put real credentials in `/sandbox/config.json`, never in the baked-in `config.json`.
- `mediaconvert.proxyPriority` must be **higher** than `priority`, or splitting the
  submission buys nothing.
- The sweep does nothing on the file backend, by design. `main.rs` says so at startup.
- A `202` is a normal answer from the playback routes, not an error. Clients that
  treat it as one will show failures for videos that are merely still transcoding.
- `verified: false` on a resolved URL means the rung was picked on the file extension
  because no probe was stored — ffprobe was missing or failed. Check the logs; the
  service degrades rather than refusing uploads.
- An absent `originalS3Path` is normal for an already-progressive upload. It is only
  a problem when `playback.ready` is also empty.
- Database migrations run automatically when `videoMetadata.storage = "db"`.
