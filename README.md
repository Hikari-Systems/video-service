# video-service-rs

A Rust/actix-web microservice for video upload, transcoding and delivery via AWS S3,
AWS Elemental MediaConvert and CloudFront. Built to the same shape as
`image-service-rs` — same config layering, same metadata backends, same deployment —
with the differences video actually forces.

---

## What is different from image-service, and why

Three things, and they drive everything else in this repo.

### 1. Transcoding is remote and asynchronous

ImageMagick is a subprocess: you call it, it returns, the file exists. A MediaConvert
job is accepted in milliseconds and completes minutes later, long after the request
that submitted it has gone.

So the `POST` that asks for renditions returns `202` with a job id, and the
**transcode sweep** is what writes results back. In the image service the sweep is a
convenience; here it is structural. With it off, jobs are submitted and their outputs
are never recorded — the service says so at startup.

### 2. The upload may not be playable

The image service's fallback chain — exact size, then original, then the raw upload —
can redirect blindly, because every rung is a file a browser renders. A JPEG is a
JPEG.

Video breaks that in two independent ways:

| | What it looks like | What fixes it |
|---|---|---|
| **Undecodable codec/container** — ProRes, MKV, AVI, HEVC outside Safari | A dead player | Nothing local. Wait for MediaConvert. |
| **Index (`moov`) at the end of the file** — the default for most phone and camera muxers | A spinner that never resolves | A stream-copy remux. Seconds, no re-encode. |

So every upload is probed, the remux runs inline when it will help, and the fallback
chain is **checked** rather than blind. When no rung qualifies the answer is `202`
with a status body — never a redirect to a file that will not play. A client that is
told `pending` shows a spinner; a client handed a broken URL cannot tell "broken"
from "not finished".

### 3. A job's outputs are released together

There is no way to have the small rendition appear early *within* a job that also
produces the large one. Getting something in front of a viewer quickly therefore
means submitting the cheap rendition as its **own job** at a higher queue priority —
`transcode.proxyRendition`. That is the same two-pass shape as the image service's
`?forceImmediateResize=original,small` followed by
`POST .../transcode?sizes=medium,large`, expressed in the only way MediaConvert
allows.

---

## The playback fallback chain

`GET /api/video/r/:id/:rendition` walks this, and stops at the first rung that will
actually play:

1. **The exact rendition asked for.**
2. **The category's other renditions**, in `transcode.fallbackOrder` (cheapest
   first — a viewer waiting on `hd` would rather watch `proxy` now).
3. **The faststart remux**, when one was needed and succeeded.
4. **The raw upload**, if the probe cleared it (`browserPlayable && faststart`). An
   upload that arrived already progressive is never copied anywhere — this rung is
   what answers for it.
5. **The caller's own `sourceUrl`**.

Nothing playable → **`202 Accepted`** with `Retry-After` and a status body:

```json
{
  "id": "…",
  "requested": "hd",
  "reason": "transcoding",
  "retryAfter": 5,
  "playback": {
    "state": "pending",
    "ready": [],
    "pending": ["proxy", "sd", "hd"],
    "percentComplete": 42
  }
}
```

`reason` is one of `transcoding`, `source_not_playable`, `not_started`, `no_source` —
enough for a client to say something true while it waits.

---

## API

### `GET /healthcheck`
`200 OK`, body `OK`.

---

### `POST /api/video/:category`
Multipart upload. Field name `video` (or `file`).

| Query param | Description |
|---|---|
| `renditions` | Comma-separated renditions to submit, or a boolean spelling (`true`/`yes`/`1`/`all`, `false`/`no`/`0`/`none`). Absent follows `transcode.processing`. Governs **renditions only** — see below. |
| `defer` | Comma-separated renditions to skip. Subtracted from `renditions`, and wins over it. The only way to skip the remux. |

What happens inline, before the response: the source is probed, stored, and — if a
stream copy will make it playable — remuxed. What happens afterwards: everything
else. A selection decides what gets *submitted*, not what gets finished.

**`original` is not subject to the selection.** It is produced unless `?defer=original`
explicitly asks otherwise, whatever `renditions` or `transcode.processing` say. This
is the one place image-service's semantics could not be carried over: there,
deferring the original saves a re-encode and a later request produces it; here it is
a byte copy or a stream copy, it is the only thing that makes an upload playable
before the queue drains, and **nothing would ever come back for it** — the sweep
submits MediaConvert jobs, it does not run remuxes. Deferring it by default meant
every upload landed unplayable and stayed that way, which is exactly the gap the
original exists to close.

So `?renditions=proxy` stores, probes and remuxes now, and queues only the cheap
rendition. `?defer=original` is how you opt out of the remux.

**`201 Created`** — the descriptor (see below).

---

### `POST /api/video/:category/url`
Body `{"url": "https://…"}`. Same pipeline once the bytes are local. Worth having
because a video large enough to be worth transcoding is usually already somewhere
addressable, and round-tripping it through the caller wastes two transfers.

---

### `GET /api/video/:id`
The full descriptor: the stored record, a signed URL for the source, and the playback
status block.

```json
{
  "id": "6a2ecdd6-…",
  "category": "clip",
  "downloadedS3Path": "clip-6a2ecdd6-….mov",
  "originalS3Path": "clip-6a2ecdd6-…-original.mp4",
  "renditionFiles": [
    { "name": "proxy", "s3Path": "clip-6a2ecdd6-…-proxy.mp4", "mimeType": "video/mp4", "kind": "progressive", "width": 640, "height": 360 }
  ],
  "probe": {
    "container": "mov", "videoCodec": "h264", "audioCodec": "aac",
    "durationSeconds": 42.5, "width": 1920, "height": 1080,
    "codecsPlayable": true, "browserPlayable": false, "faststart": false
  },
  "jobs": [
    { "id": "1234-abcd", "status": "PROGRESSING", "renditions": ["sd","hd"], "submittedAt": "…", "percentComplete": 42 }
  ],
  "originalFileUrl": "https://cdn.example.com/…?Expires=…&Signature=…",
  "playback": { "state": "ready", "ready": ["proxy","original"], "pending": ["sd","hd"], "reason": "transcoding" }
}
```

Read the `probe` block when something is not playing. `codecsPlayable: true` with
`browserPlayable: false` is a container problem the remux fixes; `codecsPlayable:
false` means only MediaConvert can help.

`originalS3Path` is absent for an upload that arrived already progressive — there was
nothing to fix and nothing was copied. `playback.ready` will name `source` instead.

---

### `GET /api/video/:id/status`
The playback block alone, for polling. `200` when something is playable, `202` with
`Retry-After` when not.

---

### `GET /api/video/r/:id/:rendition`
`302` to a playable URL, with `X-Video-Rendition` naming what you actually got (not
always what you asked for). `202` when nothing is playable yet. `404` if no such
video.

### `GET /api/video/s/:id/:rendition`
The same resolution as JSON:

```json
{
  "url": "https://cdn.example.com/clip-…-proxy.mp4?Expires=…",
  "rendition": "proxy",
  "exact": false,
  "kind": "progressive",
  "verified": true,
  "cookiePath": null,
  "playback": { "…": "…" }
}
```

`verified: false` means the rung was chosen on the file extension because no probe was
stored. It may well play; nothing has confirmed it will.

---

### `GET /api/video/:id/cookies/:rendition`
CloudFront **signed cookies** for an HLS rendition, scoped to the prefix its manifest
and segments share.

This exists because a signed URL cannot do the job. It authorises exactly the request
carrying its query string; an HLS player then issues its own requests for each
segment, with no query string, and every one of those would 403. A custom policy over
a wildcard resource, delivered as cookies, covers the manifest and its segments
together.

Returns `{"signed": false, "manifestUrl": "…"}` when signing is disabled — the local
MinIO case, where objects are public and there is nothing to authorise.

---

### `POST /api/video/:id/transcode`
Submit jobs for an existing video. Accepts `?renditions=` and `?defer=`.

Defaults to what is actually **outstanding** rather than re-submitting everything: a
job for a rendition that already exists is a duplicate bill for a file we have.

**`202 Accepted`** with the submitted list and job ids. `200` with an empty
`submitted` when there was nothing to do. `503` when MediaConvert is not configured.

---

### `GET /api/category/list`
Configured categories, their renditions, and the fallback order each will use.

---

### `GET /test`
A test page: upload, resolve, poll. It treats `202` as a normal answer, which is the
behaviour a real client wants to copy.

---

## Configuration

Loaded in priority order (lowest → highest):

1. **`config.json`** — baked into the image
2. **`/sandbox/config.json`** — volume-mounted override for secrets; silently skipped if absent
3. **Environment variables** — `__` as path separator, exact camelCase keys

All key names are **camelCase and case-sensitive**.

### Keys that matter

| Key | Description |
|---|---|
| `videoMetadata.storage` | `"file"` (JSON on disk) or `"db"` (PostgreSQL). The sweep needs `db`. |
| `mediaconvert.roleArn` | The role MediaConvert assumes to read the input and write outputs. **Empty disables submission** — the service still starts and serves. |
| `mediaconvert.queueArn` | Optional; the account's Default queue is used otherwise. |
| `mediaconvert.priority` / `proxyPriority` | Queue priority, `-50..=50`, higher runs sooner. `proxyPriority` must exceed `priority` or splitting the job buys nothing. |
| `mediaconvert.accelerated` | Accelerated transcoding, submitted as `PREFERRED` so an unsupported input runs unaccelerated instead of failing. |
| `transcode.processing` | `"deferred"` (default) submits nothing on upload; anything else submits everything. An explicit `?renditions=` wins either way. |
| `transcode.renditionKeys` | Comma-separated renditions for the `default` category. |
| `transcode.renditionSets` | `{ "categoryName": "key1,key2" }` for other categories. Looked up case-insensitively. |
| `transcode.fallbackOrder` | Order the playback chain tries renditions in. Cheapest first. Anything omitted is still reachable, appended in category order. |
| `transcode.proxyRendition` | The rendition submitted as its own expedited job. Empty submits one job for everything. |
| `transcode.transcodeSweep.enabled` | **Turn this on**, or submitted jobs are never recorded. |
| `cloudfront.cookieDomain` | Domain for the HLS playback cookies. Empty leaves them host-only, which is right when the player is served from the CDN domain. |
| `ffmpeg.bin` / `ffmpeg.probeBin` | Used for the probe and the remux only. This service never encodes. |

### Renditions

```json
"proxy": { "width": 640, "height": 360, "bitrate": 800000, "kind": "progressive", "mimeType": "video/mp4", "extension": ".mp4" }
```

`bitrate` is a QVBR **ceiling**, not a target — a static talking head costs a fraction
of it. A rendition with no `width`/`height` produces no output rather than silently
inheriting the source's size.

For HLS, set `kind: "hls"` and a `ladder`:

```json
"hls": { "kind": "hls", "ladder": "640x360@800000,1280x720@2500000", "segmentSeconds": 6 }
```

`original` and `transcodeSweep` are **reserved words**, not renditions. They are named
fields consumed by serde before the flattened rendition map, so they can never appear
in `renditionKeys` or a `renditionSets` entry.

---

## The transcode sweep

Two phases per pass, reconcile first.

**Reconcile** polls the jobs already running and records what finished. **Submit**
hands MediaConvert the videos that still need work. Reconcile goes first because a
job discovered complete this pass stops being a submission candidate in the same
pass, and because a viewer waiting on a rendition that landed thirty seconds ago
should not wait another interval.

Run it in-process (`transcode.transcodeSweep.enabled`) or from a scheduler:

```bash
video-service transcode-sweep [batch]
```

The subcommand is deliberately **not** gated on `enabled`, so the "turn the loop off
and drive it from cron" setup works. It takes the same lease as the loop, so running
it by hand while the loop is on is safe.

Both need `videoMetadata.storage = "db"`. Only Postgres can claim a video
exclusively, and the stake is higher than for images: two nodes that duplicate an
ImageMagick run waste CPU, whereas two nodes that duplicate a submission produce two
MediaConvert jobs and two invoices.

The claim is a **lease, not a flag**. A flag is only correct if whoever sets it lives
to clear it, and on a spot fleet a node can vanish at any moment.

---

## Local development

```bash
docker compose up --build
```

Everything up to and including the probe, the remux and the playback fallback works
offline — which is most of what makes this service interesting. MediaConvert has no
local stand-in, so renditions need a real `roleArn`; without one the service starts,
serves, and reports `pending` with reason `not_started`.

To run on the host against the MinIO container:

```bash
docker compose up -d minio minio-init
source scripts/local-env.sh
cargo run
```

---

## AWS prerequisites

- A **MediaConvert service role** that can read and write the bucket. Jobs submitted
  without one fail at AWS, not here.
- MediaConvert is billed **per output minute, per rendition**. Three renditions cost
  roughly three times one — worth modelling before fixing `renditionKeys`.
- The regional endpoint (`mediaconvert.<region>.amazonaws.com`) works directly;
  the old per-account `DescribeEndpoints` discovery is not needed.
