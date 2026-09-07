use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::video::{
    ExpectedRenditions, Rendition, SourceProbe, TranscodeJob, VideoBackend, VideoRecord,
};

pub struct DbBackend {
    pool: PgPool,
}

impl DbBackend {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Every column, in the order every query returns them.
const COLUMNS: &str = "id, category, source_url, downloaded_s3_path, original_s3_path, \
                       rendition_files, probe, jobs, avoid_transcode_until, created_at";

#[derive(FromRow)]
struct VideoRow {
    id: Uuid,
    category: Option<String>,
    source_url: Option<String>,
    downloaded_s3_path: Option<String>,
    original_s3_path: Option<String>,
    rendition_files: Option<Value>,
    probe: Option<Value>,
    jobs: Option<Value>,
    avoid_transcode_until: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
}

fn row_to_record(row: VideoRow) -> Result<VideoRecord> {
    let rendition_files: Option<Vec<Rendition>> = match row.rendition_files {
        Some(v) => Some(serde_json::from_value(v).context("Failed to deserialise rendition_files")?),
        None => None,
    };
    let probe: Option<SourceProbe> = match row.probe {
        Some(v) => serde_json::from_value(v).context("Failed to deserialise probe")?,
        None => None,
    };
    let jobs: Vec<TranscodeJob> = match row.jobs {
        Some(v) => serde_json::from_value(v).context("Failed to deserialise jobs")?,
        None => Vec::new(),
    };
    Ok(VideoRecord {
        id: Some(row.id),
        category: row.category,
        source_url: row.source_url,
        downloaded_s3_path: row.downloaded_s3_path,
        original_s3_path: row.original_s3_path,
        rendition_files,
        probe,
        jobs,
        avoid_transcode_until: row.avoid_transcode_until,
        created_at: row.created_at,
    })
}

/// The containment tests that find a row with a job still running.
///
/// Written as `@>` containment rather than a `jsonb_array_elements` subquery
/// specifically so the GIN index can serve it — the subquery form is correct and
/// unindexable, which on a table that only ever grows is the difference between a
/// constant-time claim and a full scan every minute on every replica.
const IN_FLIGHT: &str = r#"(jobs @> '[{"status":"SUBMITTED"}]'::jsonb
                         OR jobs @> '[{"status":"PROGRESSING"}]'::jsonb)"#;

#[async_trait]
impl VideoBackend for DbBackend {
    /// Claim videos that still need a job submitted.
    ///
    /// Each clause earns its place, and most of the reasoning is the image service's:
    ///
    /// * **`FOR UPDATE SKIP LOCKED`** is what makes this safe on a fleet. Two nodes
    ///   running it at the same instant step over each other's in-flight rows
    ///   instead of blocking, so neither waits and neither gets a duplicate. Here
    ///   the cost of getting it wrong is not just wasted CPU — a duplicate claim is
    ///   a duplicate MediaConvert job, which is billed.
    /// * **The lease is written in the same statement as the selection**, so there
    ///   is no window between reading a row and marking it.
    /// * **`NOT IN_FLIGHT`** keeps the sweep from submitting a second job for work
    ///   that is already running. This is the clause that makes the claim safe to
    ///   run alongside an upload that has just submitted.
    /// * **Incompleteness is per category**, so a 2-rendition `clip` is not judged
    ///   against a 3-rendition `feature`, and a finished row stops matching instead
    ///   of being re-claimed forever.
    async fn claim_for_submit(
        &self,
        limit: i64,
        lease_seconds: i64,
        expected: &[ExpectedRenditions],
        default_expected: i32,
    ) -> Result<Vec<VideoRecord>> {
        let cats: Vec<String> = expected.iter().map(|e| e.category.clone()).collect();
        let counts: Vec<i32> = expected.iter().map(|e| e.count).collect();

        let sql = format!(
            r#"
            UPDATE video SET avoid_transcode_until = now() + make_interval(secs => $1)
            WHERE id IN (
                SELECT v.id
                FROM video v
                LEFT JOIN unnest($2::text[], $3::int[]) AS e(cat, n)
                       ON lower(coalesce(v.category, '')) = e.cat
                WHERE v.downloaded_s3_path IS NOT NULL
                  AND (v.avoid_transcode_until IS NULL OR v.avoid_transcode_until <= now())
                  AND NOT {IN_FLIGHT}
                  AND jsonb_array_length(coalesce(v.rendition_files, '[]'::jsonb))
                      < coalesce(e.n, $4)
                ORDER BY v.created_at NULLS FIRST
                LIMIT $5
                FOR UPDATE OF v SKIP LOCKED
            )
            RETURNING {COLUMNS}
            "#,
            IN_FLIGHT = IN_FLIGHT.replace("jobs", "v.jobs"),
            COLUMNS = COLUMNS
        );

        let rows = sqlx::query_as::<_, VideoRow>(&sql)
            .bind(lease_seconds as f64)
            .bind(&cats)
            .bind(&counts)
            .bind(default_expected)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .context("DB error claiming videos for submit")?;

        rows.into_iter().map(row_to_record).collect()
    }

    /// Claim videos with a job still running, so it can be polled.
    ///
    /// Ordered by `avoid_transcode_until` rather than `created_at`: what matters is
    /// which row has gone longest without a look, not which video is oldest.
    async fn claim_for_reconcile(&self, limit: i64, lease_seconds: i64) -> Result<Vec<VideoRecord>> {
        let sql = format!(
            r#"
            UPDATE video SET avoid_transcode_until = now() + make_interval(secs => $1)
            WHERE id IN (
                SELECT v.id
                FROM video v
                WHERE (v.avoid_transcode_until IS NULL OR v.avoid_transcode_until <= now())
                  AND {IN_FLIGHT}
                ORDER BY v.avoid_transcode_until NULLS FIRST
                LIMIT $2
                FOR UPDATE OF v SKIP LOCKED
            )
            RETURNING {COLUMNS}
            "#,
            IN_FLIGHT = IN_FLIGHT.replace("jobs", "v.jobs"),
            COLUMNS = COLUMNS
        );

        let rows = sqlx::query_as::<_, VideoRow>(&sql)
            .bind(lease_seconds as f64)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .context("DB error claiming videos for reconcile")?;

        rows.into_iter().map(row_to_record).collect()
    }

    async fn set_transcode_lease(&self, id: Uuid, until: Option<DateTime<Utc>>) -> Result<()> {
        sqlx::query("UPDATE video SET avoid_transcode_until = $2 WHERE id = $1")
            .bind(id)
            .bind(until)
            .execute(&self.pool)
            .await
            .context("DB error setting transcode lease")?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<VideoRecord>> {
        let uuid: Uuid = id.parse().context("Invalid UUID format")?;
        let sql = format!("SELECT {COLUMNS} FROM video WHERE id = $1");

        let row = sqlx::query_as::<_, VideoRow>(&sql)
            .bind(uuid)
            .fetch_optional(&self.pool)
            .await
            .context("DB error fetching video")?;

        match row {
            Some(r) => Ok(Some(row_to_record(r)?)),
            None => Ok(None),
        }
    }

    async fn upsert(&self, video: VideoRecord) -> Result<VideoRecord> {
        let id = video.id.unwrap_or_else(Uuid::new_v4);
        let renditions: Option<Value> = match &video.rendition_files {
            Some(v) => Some(serde_json::to_value(v).context("Failed to serialise rendition_files")?),
            None => None,
        };
        let probe: Option<Value> = match &video.probe {
            Some(p) => Some(serde_json::to_value(p).context("Failed to serialise probe")?),
            None => None,
        };
        let jobs = serde_json::to_value(&video.jobs).context("Failed to serialise jobs")?;

        let sql = format!(
            r#"
            INSERT INTO video (
                id, category, source_url, downloaded_s3_path, original_s3_path,
                rendition_files, probe, jobs, avoid_transcode_until, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now())
            ON CONFLICT (id) DO UPDATE SET
                category              = EXCLUDED.category,
                source_url            = EXCLUDED.source_url,
                downloaded_s3_path    = EXCLUDED.downloaded_s3_path,
                original_s3_path      = EXCLUDED.original_s3_path,
                rendition_files       = EXCLUDED.rendition_files,
                probe                 = EXCLUDED.probe,
                jobs                  = EXCLUDED.jobs,
                avoid_transcode_until = EXCLUDED.avoid_transcode_until,
                updated_at            = now()
            RETURNING {COLUMNS}
            "#
        );

        let row = sqlx::query_as::<_, VideoRow>(&sql)
            .bind(id)
            .bind(&video.category)
            .bind(&video.source_url)
            .bind(&video.downloaded_s3_path)
            .bind(&video.original_s3_path)
            .bind(renditions)
            .bind(probe)
            .bind(jobs)
            .bind(video.avoid_transcode_until)
            .fetch_one(&self.pool)
            .await
            .context("DB error upserting video")?;

        row_to_record(row)
    }
}
