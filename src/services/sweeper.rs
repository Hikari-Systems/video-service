//! The background pass that turns submitted jobs into playable renditions.
//!
//! It carries more weight here than the image service's equivalent does. There, the
//! sweep is a convenience: it finishes variants an upload chose to defer, and if it
//! never ran, an explicit `POST .../transcode` would still produce them. Here it is
//! **structural**. A MediaConvert job completes minutes after the request that
//! submitted it has returned; without something that comes back to look, nothing
//! ever writes the outputs into the record, and the renditions exist in S3 that no
//! caller can find.
//!
//! ## Two phases, reconcile first
//!
//! * **Reconcile** — poll the jobs already running and record what finished. Cheap
//!   (one API call each), and it is what makes renditions appear.
//! * **Submit** — hand MediaConvert the videos that still need work.
//!
//! Reconcile runs first because a completed job discovered this pass is a row that
//! stops being a candidate for submission in the same pass, and because a viewer
//! waiting on a rendition that finished thirty seconds ago should not wait another
//! interval for it.
//!
//! ## Running on more than one node
//!
//! Every replica runs this loop, so the claim must be exclusive. Exclusivity lives
//! entirely in the `UPDATE … FOR UPDATE SKIP LOCKED` statements in
//! [`crate::models::video_db`]. The stake is higher than for images: two nodes that
//! duplicate an ImageMagick run waste CPU, whereas two nodes that duplicate a
//! submission produce two MediaConvert jobs and two invoices.
//!
//! The claim is a **lease**, not an in-progress flag. A flag is only correct if
//! whoever sets it always lives to clear it, and on a spot fleet a node can vanish
//! at any moment. A lease that expires needs no cleanup and no operator.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use tracing::{debug, error, info, warn};

use crate::config::TranscodeConfig;
use crate::helpers::transcode::{missing_renditions, reconcile_job, submit_jobs};
use crate::models::video::ExpectedRenditions;
use crate::state::AppState;

/// How long to park a video the sweep claimed but found nothing to do for.
///
/// Reachable when a row's rendition *count* is short but every key its category
/// configures is already present — a shape mismatch rather than missing work.
/// Clearing the lease would re-claim it on the next pass forever.
const NOTHING_TO_DO_PARK_HOURS: i64 = 24;

/// Spawn the sweep loop. Returns immediately; the task runs for the process's life.
pub fn spawn(state: Arc<AppState>) {
    let cfg = state.config.transcode.transcode_sweep.clone();
    let interval = Duration::from_secs(cfg.interval_seconds.max(1) as u64);

    info!(
        "transcode sweep: on — every {}s, up to {} submission(s) ({}s lease) and {} \
         reconcile(s) ({}s poll) per pass",
        cfg.interval_seconds,
        cfg.batch_size,
        cfg.lease_seconds,
        cfg.reconcile_batch_size,
        cfg.poll_seconds
    );

    tokio::spawn(async move {
        // Offset the first pass so a whole fleet restarting together does not wake
        // into the same database at once.
        tokio::time::sleep(interval).await;
        loop {
            if let Err(e) = pass(&state).await {
                // Never fatal: a sweep that fails is a sweep that runs again.
                error!("transcode sweep: pass failed: {e:#}");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Run a single pass and return, for the `video-service transcode-sweep [n]`
/// subcommand.
///
/// Deliberately **not** gated on `transcodeSweep.enabled`: that flag decides whether
/// every replica runs the loop on its own, which is a different question from
/// whether an operator (or an external scheduler) may ask for one pass now. Gating
/// it would make the obvious "turn the loop off and drive it from cron" setup
/// impossible.
pub async fn run_once(state: &AppState, batch: Option<u32>) -> anyhow::Result<()> {
    if state.config.video_metadata.storage.trim() != "db" {
        anyhow::bail!(
            "transcode sweep needs videoMetadata.storage = \"db\": only the Postgres \
             backend can claim a video exclusively, so on {:?} two callers would \
             submit the same MediaConvert job rather than skip it",
            state.config.video_metadata.storage
        );
    }

    let cfg = &state.config.transcode.transcode_sweep;
    let reconciled = reconcile_pass(state, cfg.reconcile_batch_size).await?;
    let submitted = submit_pass(state, batch.unwrap_or(cfg.batch_size)).await?;

    // Silent on an idle pass, and deliberately so: this runs from cron as often as
    // once a minute, and a job that says "nothing to do" 1,440 times a day buries
    // the times it did something.
    if reconciled > 0 || submitted > 0 {
        info!("transcode sweep: one-shot pass complete, {reconciled} reconciled, {submitted} submitted");
    } else {
        debug!("transcode sweep: one-shot pass complete, nothing pending");
    }
    Ok(())
}

async fn pass(state: &AppState) -> anyhow::Result<()> {
    let cfg = &state.config.transcode.transcode_sweep;
    reconcile_pass(state, cfg.reconcile_batch_size).await?;
    submit_pass(state, cfg.batch_size).await?;
    Ok(())
}

/// Poll the jobs that are running and write back whatever finished.
async fn reconcile_pass(state: &AppState, batch: u32) -> anyhow::Result<usize> {
    let cfg = &state.config.transcode.transcode_sweep;
    // `poll_seconds`, NOT `lease_seconds`. This claim answers "when should this job
    // be looked at again?", which has nothing to do with how long a submission must
    // be protected from a duplicate.
    let claimed = state
        .backend
        .claim_for_reconcile(batch.max(1) as i64, cfg.poll_seconds.max(1) as i64)
        .await?;

    if claimed.is_empty() {
        debug!("transcode sweep: no jobs in flight");
        return Ok(0);
    }
    info!("transcode sweep: polling {} video(s) with jobs in flight", claimed.len());

    let mut done = 0usize;
    for mut video in claimed {
        let Some(id) = video.id else { continue };
        let in_flight: Vec<String> = video.in_flight_jobs().map(|j| j.id.clone()).collect();

        let mut changed = false;
        let mut still_running = false;
        for job_id in in_flight {
            match state.mediaconvert.job_state(&job_id).await {
                Ok(job_state) => {
                    let running = job_state.status == "SUBMITTED" || job_state.status == "PROGRESSING";
                    still_running |= running;
                    match reconcile_job(&mut video, &job_state, state).await {
                        Ok(wrote) => changed |= wrote || !running,
                        Err(e) => warn!("transcode sweep: {id} could not reconcile {job_id}: {e:#}"),
                    }
                }
                Err(e) => {
                    // A poll that fails tells us nothing about the job, so leave it
                    // in flight and try again after the lease.
                    warn!("transcode sweep: {id} could not poll job {job_id}: {e:#}");
                    still_running = true;
                }
            }
        }

        if changed {
            match state.backend.upsert(video).await {
                Ok(_) => {
                    done += 1;
                    info!("transcode sweep: {id} updated from job status");
                }
                Err(e) => error!("transcode sweep: {id} reconciled but not saved: {e:#}"),
            }
        }

        // Clear the lease when nothing is running any more, so the submit phase can
        // pick the row up immediately if it is still short of renditions. While a job
        // is running, leave the claim's lease in place: at `poll_seconds` it is the
        // poll interval, and it keeps a fleet from all polling the same job at once.
        if !still_running {
            if let Err(e) = state.backend.set_transcode_lease(id, None).await {
                warn!("transcode sweep: {id} lease not cleared: {e:#}");
            }
        }
    }
    Ok(done)
}

/// Submit jobs for videos that still need renditions.
async fn submit_pass(state: &AppState, batch: u32) -> anyhow::Result<usize> {
    if !state.mediaconvert.is_available() {
        debug!("transcode sweep: MediaConvert is not configured — skipping the submit phase");
        return Ok(0);
    }

    let cfg = &state.config.transcode.transcode_sweep;
    let (expected, default_expected) = expected_renditions(&state.config.transcode);

    let claimed = state
        .backend
        .claim_for_submit(
            batch.max(1) as i64,
            cfg.lease_seconds.max(1) as i64,
            &expected,
            default_expected,
        )
        .await?;

    if claimed.is_empty() {
        debug!("transcode sweep: nothing to submit");
        return Ok(0);
    }
    info!("transcode sweep: claimed {} video(s) to submit", claimed.len());

    let mut submitted = 0usize;
    for video in claimed {
        let Some(id) = video.id else { continue };
        let missing = missing_renditions(&video, state);
        if missing.is_empty() {
            warn!(
                "transcode sweep: {id} was claimed but has every configured rendition \
                 — parking it for {NOTHING_TO_DO_PARK_HOURS}h"
            );
            let park = Utc::now() + ChronoDuration::hours(NOTHING_TO_DO_PARK_HOURS);
            let _ = state.backend.set_transcode_lease(id, Some(park)).await;
            continue;
        }

        info!("transcode sweep: {id} submitting {missing:?}");
        // `submit_jobs` records the ids and releases the lease in one statement, so
        // the reconcile phase can pick the row up on the next pass rather than
        // waiting out a lease sized for a whole transcode. On failure it leaves the
        // lease alone, where it doubles as the retry backoff.
        match submit_jobs(video, &missing, state).await {
            Ok(_) => submitted += 1,
            Err(e) => error!("transcode sweep: {id} submission failed, will retry after lease: {e:#}"),
        }
    }
    Ok(submitted)
}

/// The (category, expected-rendition-count) pairs the claim query judges against,
/// plus the count for a category with no configured set.
///
/// These are **rendition keys only, and deliberately exclude the original**: the
/// query compares them against `jsonb_array_length(rendition_files)`, and the
/// original is not a rendition — it lives in its own column. Counting it would make
/// every finished row look one short and re-claim it forever.
///
/// Note also what is *not* here: no clause requires `original_s3_path` to be set.
/// A source no browser can decode legitimately has no playable original, ever, and
/// judging completeness on one would re-claim every ProRes upload until the end of
/// time.
fn expected_renditions(transcode: &TranscodeConfig) -> (Vec<ExpectedRenditions>, i32) {
    let expected = transcode
        .rendition_sets
        .keys()
        .map(|category| ExpectedRenditions {
            category: category.to_lowercase(),
            count: transcode.rendition_keys_for_category(&category.to_lowercase()).len() as i32,
        })
        .collect();
    let default_expected = transcode.rendition_keys_for_category("").len() as i32;
    (expected, default_expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcode_config(keys: &str, sets: &[(&str, &str)]) -> TranscodeConfig {
        TranscodeConfig {
            rendition_keys: keys.to_string(),
            rendition_sets: sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    /// The counts handed to the claim query are rendition keys only. Including the
    /// original would leave every finished row one short and re-claim it forever —
    /// and every re-claim is a duplicate MediaConvert job.
    #[test]
    fn expected_counts_exclude_the_original() {
        let cfg = transcode_config("proxy,sd", &[("feature", "proxy,sd,hd")]);
        let (expected, default_expected) = expected_renditions(&cfg);
        assert_eq!(default_expected, 2, "global renditionKeys has two entries");
        let f = expected.iter().find(|e| e.category == "feature").unwrap();
        assert_eq!(f.count, 3, "three rendition keys, original not counted");
    }

    /// Categories are lower-cased before their set is looked up, so the pairs the
    /// query joins on must be lower-cased too or a `renditionSets` key with a
    /// capital in it would never match.
    #[test]
    fn expected_categories_are_lower_cased() {
        let cfg = transcode_config("proxy", &[("shortClip", "proxy,sd")]);
        let (expected, _) = expected_renditions(&cfg);
        assert_eq!(expected[0].category, "shortclip");
        assert_eq!(expected[0].count, 2);
    }

    #[test]
    fn an_unconfigured_category_uses_the_global_count() {
        let cfg = transcode_config("proxy,sd,hd", &[]);
        let (expected, default_expected) = expected_renditions(&cfg);
        assert!(expected.is_empty());
        assert_eq!(default_expected, 3);
    }
}
