CREATE TABLE IF NOT EXISTS video (
    id                    UUID PRIMARY KEY NOT NULL,
    category              VARCHAR(255),
    source_url            TEXT,
    -- The raw upload, byte for byte. Always written; not always playable.
    downloaded_s3_path    TEXT,
    -- A copy of the source that WILL play — either the upload itself when it was
    -- already progressive, or its faststart remux. Legitimately NULL forever when
    -- the source codecs are ones no browser decodes, which is why (unlike the image
    -- service) it takes no part in the completeness test below.
    original_s3_path      TEXT,
    rendition_files       JSONB,
    -- What ffprobe found: container, codecs, dimensions, and the two booleans the
    -- playback fallback turns on.
    probe                 JSONB,
    -- MediaConvert jobs, in flight and finished. An array rather than a column,
    -- because a submission is split into a fast proxy job and a slower ladder job.
    jobs                  JSONB,
    avoid_transcode_until TIMESTAMPTZ,
    created_at            TIMESTAMPTZ,
    updated_at            TIMESTAMPTZ
);

-- Index for the sweep's SUBMIT claim.
--
-- The claim looks for rows that are eligible (no lease, or an expired one) and
-- short of renditions, oldest first. Without an index that is a full scan of
-- `video` on every pass, on every replica, forever.
--
-- Partial, on the predicate that actually narrows it: a row with no source cannot
-- be transcoded, so it can never be claimed and does not belong in the index.
-- `avoid_transcode_until NULLS FIRST` matches the eligibility test (never leased
-- sorts before leased) and `created_at` matches the ORDER BY, so the claim walks
-- the index in order and stops at LIMIT.
CREATE INDEX IF NOT EXISTS video_transcode_submit_idx
    ON video (avoid_transcode_until NULLS FIRST, created_at NULLS FIRST)
    WHERE downloaded_s3_path IS NOT NULL;

-- Index for the sweep's RECONCILE claim.
--
-- That claim asks "which rows have a job still running?", which is a containment
-- test against the jobs array — `jobs @> '[{"status":"PROGRESSING"}]'` — and GIN
-- with jsonb_path_ops is what makes containment indexable. Without it, every pass
-- would deserialise every row's job history to find the handful still in flight.
CREATE INDEX IF NOT EXISTS video_jobs_gin_idx
    ON video USING gin (jobs jsonb_path_ops);
