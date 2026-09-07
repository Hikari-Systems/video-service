mod config;
mod helpers;
mod models;
mod routes;
mod services;
mod state;

use actix_web::{middleware, web, App, HttpResponse};
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

use config::AppConfig;
use models::video::VideoBackend;
use models::video_db::DbBackend;
use models::video_file::FileBackend;
use services::cloudfront::CloudfrontService;
use services::downloader::DownloaderService;
use services::ffmpeg::FfmpegService;
use services::ffprobe::FfprobeService;
use services::mediaconvert::MediaConvertService;
use services::s3::S3Service;
use state::AppState;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    hs_utils::healthcheck::check_subcommand(
        AppConfig::load().map(|c| c.server.port).unwrap_or(3000),
    );

    if let Err(e) = run().await {
        eprintln!("Fatal error: {:#}", e);
        std::process::exit(1);
    }
    Ok(())
}

/// `argv[1] == "transcode-sweep"` → `Some(batch)`, from the optional positional
/// `argv[2]`. Returns `None` for a normal server start.
///
/// Parsed by hand rather than handed to a CLI crate because this is the second
/// subcommand in the binary and the first (`healthcheck`) is argv-matched too — a
/// dependency would be more machinery than the feature.
fn transcode_sweep_subcommand() -> Option<Option<u32>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("transcode-sweep") {
        return None;
    }
    // A value that is present but unparseable is a typo in a cron line. Say so;
    // still run, because refusing to sweep over a malformed argument is the worse
    // failure.
    let batch = args.next().and_then(|raw| match raw.parse() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!("transcode-sweep: ignoring unparseable batch size {raw:?}");
            None
        }
    });
    Some(batch)
}

async fn run() -> Result<()> {
    let cfg = AppConfig::load().context("Failed to load application config")?;

    hs_utils::logging::init(&cfg.log.level);

    info!("video-service starting on port {}", cfg.server.port);

    let backend: Arc<dyn VideoBackend> = match cfg.video_metadata.storage.trim() {
        "db" => {
            info!("Using PostgreSQL backend, running migrations…");
            let pool = hs_utils::db::build_pool(&cfg.db).await?;
            run_migrations(&pool).await?;
            Arc::new(DbBackend::new(pool))
        }
        _ => {
            info!("Using file backend at {}", cfg.video_metadata.parent_path);
            Arc::new(FileBackend::new(cfg.video_metadata.parent_path.clone()))
        }
    };

    let cf_service =
        CloudfrontService::new(&cfg.cloudfront).context("Failed to initialise CloudFront service")?;

    info!("S3 bucket: {}", cfg.s3.bucket_name);
    // Worth stating at startup: an unsigned distribution serves every object to
    // anyone who can guess a key, and the difference is otherwise invisible until
    // someone checks a URL by hand.
    info!(
        "CloudFront: {} ({})",
        if cf_service.is_signing() { "signing URLs" } else { "NOT signing — URLs are public" },
        if cfg.cloudfront.url.is_empty() { "no url configured" } else { &cfg.cloudfront.url }
    );

    let app_state = web::Data::new(AppState {
        s3: S3Service::new(&cfg.s3),
        cf: cf_service,
        probe: FfprobeService::new(&cfg.ffmpeg),
        ffmpeg: FfmpegService::new(&cfg.ffmpeg),
        mediaconvert: MediaConvertService::new(&cfg.mediaconvert, &cfg.s3),
        downloader: DownloaderService::new(),
        backend,
        config: cfg.clone(),
    });

    // `video-service transcode-sweep [n]` — run one pass and exit, instead of
    // serving. Same shape as the `healthcheck` subcommand, and for the same reason:
    // it needs the service's own config, credentials and AWS clients, so a shell
    // script cannot stand in for it. It buys two things the in-process loop does not:
    //
    //   * an **external** scheduler can drive the sweep with `transcodeSweep.enabled`
    //     left false, which is the right shape when you want one node clearing the
    //     backlog on a schedule you control rather than every replica deciding for
    //     itself;
    //   * a **manual** trigger, to drain a backlog or watch the claim behave,
    //     without waiting out an interval or restarting anything.
    //
    // It takes the same lease as the loop does, so running it by hand while the loop
    // is on is safe — the two cannot pick the same video.
    if let Some(batch) = transcode_sweep_subcommand() {
        return services::sweeper::run_once(&app_state, batch).await;
    }

    // Gated here rather than inside the loop so the "it will do nothing" case is one
    // line at startup instead of silence. Only the Postgres backend can claim a
    // video exclusively; on the file backend every replica would submit its own
    // MediaConvert job for the same video, which is a duplicated bill rather than
    // merely duplicated work.
    if cfg.transcode.transcode_sweep.is_enabled() {
        if cfg.video_metadata.storage.trim() == "db" {
            services::sweeper::spawn(app_state.clone().into_inner());
        } else {
            tracing::warn!(
                "transcode sweep is enabled but videoMetadata.storage is {:?}; \
                 only the db backend can claim a video exclusively, so the sweep is \
                 disabled — set storage to \"db\" to use it",
                cfg.video_metadata.storage
            );
        }
    } else if cfg.mediaconvert.is_configured() {
        // Worth saying out loud. Unlike the image service, where a missing sweep only
        // delays variants that an explicit request could still produce, here it means
        // submitted jobs are never written back at all.
        tracing::warn!(
            "transcode sweep is off — MediaConvert jobs will be submitted but their \
             results will never be recorded. Set transcode.transcodeSweep.enabled, or \
             drive `video-service transcode-sweep` from a scheduler."
        );
    }

    let port = cfg.server.port;

    hs_utils::server::run(port, move || {
        App::new()
            // /healthcheck is polled constantly by the load balancer — keep it out
            // of the request log.
            .wrap(middleware::Logger::default().exclude("/healthcheck"))
            .app_data(app_state.clone())
            .route(
                "/healthcheck",
                web::get().to(|| async { HttpResponse::Ok().body("OK") }),
            )
            .configure(routes::configure)
            .route("/test", web::get().to(test_page))
            .route("/test/", web::get().to(test_page))
    })
    .await
}

async fn test_page() -> HttpResponse {
    static HTML: &str = include_str!("../static/index.html");
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(HTML)
}

async fn run_migrations(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("Failed to run database migrations")?;
    info!("Database migrations completed");
    Ok(())
}
