//! fastsl — fast, read-only viewer for SwanLab local logs.
//!
//! Serves the original swanboard Vue bundle (embedded) plus a swanboard-compatible
//! `/api/v1` surface backed by a fast read-only reader over the swanlog directory.
//!
//! T1 scope: CLI, embedded static hosting + SPA fallback, response envelope, and the
//! `SwanLab-Version` / `SwanLab-Process-Time` headers. API endpoints land in later tasks.

mod api;
mod db;
mod resp;
mod summary_cache;
mod uistate;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::Request,
    http::{header, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch},
    Router,
};
use clap::Parser;
use rust_embed::RustEmbed;

use crate::summary_cache::SummaryCache;
use crate::uistate::{UiState, SIDECAR_NAME};

/// swanboard version advertised in the `SwanLab-Version` header (compat with the Vue client).
const SWANLAB_VERSION: &str = "0.1.10b2";

/// Embedded Vue bundle. `build.rs` produces `frontend_dist/` (from the `frontend/` submodule in a
/// dev build, or vendored into the published crate), and `rust-embed` embeds it here.
#[derive(RustEmbed)]
#[folder = "frontend_dist/"]
struct FrontendAssets;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub logdir: Arc<PathBuf>,
    /// Cache of a tag's last logged value, keyed by "run_id/folder/mtime/size".
    /// Finished runs are immutable so their entries are effectively permanent.
    /// When `--cache-dir` is set, entries persist across restarts.
    pub summary_cache: SummaryCache,
    /// View-preference overlay (pin/hide/collapse/show) persisted to a JSON sidecar,
    /// keeping `runs.swanlab` strictly read-only.
    pub ui: Arc<UiState>,
}

#[derive(Parser, Debug)]
#[command(name = "fastsl", version, about = "Fast read-only SwanLab log viewer")]
struct Args {
    /// Path to the swanlog directory (the folder containing run-* dirs and runs.swanlab).
    #[arg(long)]
    logdir: PathBuf,

    /// Host/IP to bind.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port to listen on.
    #[arg(long, default_value_t = 5092)]
    port: u16,

    /// Optional fast cache dir (tmpfs/SSD) for the sidecar index; falls back to in-memory only.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let logdir = args
        .logdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("--logdir {:?} is not accessible: {e}", args.logdir))?;
    if !logdir.is_dir() {
        anyhow::bail!("--logdir {:?} is not a directory", logdir);
    }
    let db_path = logdir.join("runs.swanlab");
    if !db_path.exists() {
        tracing::warn!(
            "runs.swanlab not found under {:?} — is this a swanlog dir?",
            logdir
        );
    }

    // View-preference sidecar lives beside the swanlog dir (never inside runs.swanlab).
    let sidecar = logdir.join(SIDECAR_NAME);
    let summary_cache = SummaryCache::new(500_000, args.cache_dir.as_deref(), &logdir);
    let state = AppState {
        logdir: Arc::new(logdir.clone()),
        summary_cache,
        ui: Arc::new(UiState::load(sidecar)),
    };
    spawn_summary_maintenance(state.logdir.clone(), state.summary_cache.clone());

    // API surface. Envelope headers applied to every response via the layer.
    let api = Router::new()
        .route("/project", get(api::get_project))
        .route("/project/summaries", get(api::get_project_summaries))
        .route("/project/charts", get(api::get_project_charts))
        .route("/experiment/:id", get(api::get_experiment))
        .route("/experiment/:id/summary", get(api::get_experiment_summary))
        .route("/experiment/:id/chart", get(api::get_experiment_chart))
        .route("/experiment/:id/status", get(api::get_experiment_status_ep))
        .route("/experiment/:id/tag/*tag", get(api::get_experiment_tag))
        .route("/experiment/:id/recent_log", get(api::get_recent_log))
        .route("/experiment/:id/requirements", get(api::get_requirements))
        .route("/media/*path", get(api::get_media))
        // PATCH: view-preference overlay (writes only the JSON sidecar).
        .route("/namespace/:id/opened", patch(api::patch_namespace_opened))
        .route("/chart/:id/status", patch(api::patch_chart_status))
        .route("/experiment/:id/show", patch(api::patch_experiment_show))
        .layer(middleware::from_fn(api_headers))
        .with_state(state.clone());

    let app = Router::new()
        .nest("/api/v1", api)
        .fallback(spa_fallback)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("fastsl serving {:?} on http://{}", logdir, addr);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Starts T7 summary maintenance:
/// 1. immediate background pre-warm so the first dashboard visit does not synchronously scan all tags;
/// 2. `notify`-triggered refreshes for append/write bursts from running experiments;
/// 3. a conservative 30s polling fallback so updates still land if recursive watch setup fails.
///
/// This deliberately reuses the verified `/project/summaries` read path. The cache key includes the
/// last shard's mtime+size, so unchanged finished runs stay hot while appended running shards create
/// fresh entries.
fn spawn_summary_maintenance(logdir: Arc<PathBuf>, cache: SummaryCache) {
    tokio::spawn(async move {
        warm_project_summaries("startup", logdir.clone(), cache.clone()).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<&'static str>(8);
        let watcher = spawn_summary_watcher(logdir.clone(), tx.clone());
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate first tick; startup prewarm already ran.
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    warm_project_summaries("poll", logdir.clone(), cache.clone()).await;
                }
                Some(reason) = rx.recv() => {
                    warm_project_summaries(reason, logdir.clone(), cache.clone()).await;
                }
            }
            // Keep the watcher alive for the life of this task.
            let _watcher = &watcher;
        }
    });
}

fn spawn_summary_watcher(
    logdir: Arc<PathBuf>,
    tx: tokio::sync::mpsc::Sender<&'static str>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};

    let last_signal = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(5)));
    let last_signal_cb = last_signal.clone();
    let callback_tx = tx;
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| match res {
            Ok(event) if event.paths.iter().any(|p| is_summary_relevant_path(p)) => {
                let should_send = {
                    let mut last = last_signal_cb.lock().unwrap();
                    if last.elapsed() >= Duration::from_secs(2) {
                        *last = Instant::now();
                        true
                    } else {
                        false
                    }
                };
                if should_send {
                    let _ = callback_tx.try_send("notify");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("summary cache watcher event error: {e}"),
        },
        notify::Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "failed to create summary cache watcher: {e}; polling fallback remains active"
            );
            return None;
        }
    };

    if let Err(e) = watcher.watch(&logdir, RecursiveMode::Recursive) {
        tracing::warn!(
            "failed to watch {:?}: {e}; polling fallback remains active",
            logdir
        );
        return None;
    }
    tracing::info!("summary cache watcher active for {:?}", logdir);
    Some(watcher)
}

fn is_summary_relevant_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|x| x.to_str()),
        Some("log") | Some("json")
    )
}

async fn warm_project_summaries(reason: &'static str, logdir: Arc<PathBuf>, cache: SummaryCache) {
    let started = Instant::now();
    let result =
        tokio::task::spawn_blocking(move || db::prewarm_project_summaries(&logdir, &cache)).await;
    match result {
        Ok(Ok(())) => tracing::info!(
            "summary cache {reason} prewarm finished in {:.3}s",
            started.elapsed().as_secs_f64()
        ),
        Ok(Err(e)) => tracing::warn!("summary cache {reason} prewarm failed: {e}"),
        Err(e) => tracing::warn!("summary cache {reason} prewarm task failed: {e}"),
    }
}

/// Adds the `SwanLab-Version` and `SwanLab-Process-Time` headers to every API response.
async fn api_headers(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let mut resp = next.run(req).await;
    let secs = start.elapsed().as_secs_f64();
    let headers = resp.headers_mut();
    headers.insert("SwanLab-Version", HeaderValue::from_static(SWANLAB_VERSION));
    if let Ok(v) = HeaderValue::from_str(&format!("{secs:.4}")) {
        headers.insert("SwanLab-Process-Time", v);
    }
    resp
}

/// Serves embedded `/assets/*` files; every other non-API path returns `index.html`
/// so the Vue client-side router handles it (mirrors swanboard's `resp_static`).
async fn spa_fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let lookup = if path.starts_with("assets/") {
        path.to_string()
    } else {
        "index.html".to_string()
    };
    match FrontendAssets::get(&lookup) {
        Some(content) => {
            let mime = mime_guess::from_path(&lookup).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
