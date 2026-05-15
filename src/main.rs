mod api_client;
mod boost;
mod config;
mod cooldown;
mod crypto;
mod notifier;
mod state;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde_json::json;
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use config::AppConfig;
use cooldown::CooldownTracker;
use notifier::DiscordNotifier;
use state::{CycleState, CycleSummary};

const TRIGGER_RATE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct AppState {
    cfg: Arc<AppConfig>,
    cycle_state: Arc<RwLock<CycleState>>,
    trigger_tx: mpsc::Sender<()>,
    metrics: PrometheusHandle,
    started_at: Instant,
    trigger_window: Arc<Mutex<VecDeque<Instant>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().json().with_env_filter(filter).init();

    if let Err(e) = crypto::init_key() {
        error!(error = %e, "private key (SS_PRIVATE_KEY_DB / PRIVATE_KEY_DB) validation failed — exiting");
        std::process::exit(1);
    }

    let cfg = Arc::new(AppConfig::from_env().context("invalid configuration")?);

    let metrics = PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install metrics recorder")?;

    info!(
        api_endpoint = %cfg.api_endpoint,
        port = cfg.port,
        boost_interval_secs = cfg.boost_interval.as_secs(),
        cycle_deadline_secs = cfg.cycle_deadline.as_secs(),
        interval_jitter_secs = cfg.interval_jitter.as_secs(),
        app_ids = ?cfg.app_ids,
        accounts_per_cycle = cfg.accounts_per_cycle,
        cron = cfg.cron_schedule.is_some(),
        cooldown_threshold = cfg.cooldown_threshold,
        cooldown_cycles = cfg.cooldown_cycles,
        bot_api_max_retries = cfg.bot_api_max_retries,
        discord_webhook = cfg.discord_webhook_url.is_some(),
        alert_threshold_ratio = cfg.alert_threshold_ratio,
        trigger_rate_limit_per_min = cfg.trigger_rate_limit_per_min,
        public_observability = cfg.public_observability,
        dry_run = cfg.dry_run,
        report_results = cfg.report_results,
        "storebooster_starting"
    );

    let api_client = Arc::new(
        api_client::ApiClient::new(
            cfg.api_endpoint.clone(),
            cfg.auth_header_name.clone(),
            cfg.auth_header_value.clone(),
        )
        .context("failed to build API client")?,
    );

    let cycle_state = Arc::new(RwLock::new(CycleState::default()));
    let cooldown = Arc::new(CooldownTracker::new(
        cfg.cooldown_threshold,
        cfg.cooldown_cycles,
    ));
    let notifier = match cfg.discord_webhook_url.clone() {
        Some(url) => match DiscordNotifier::new(url) {
            Ok(n) => Some(Arc::new(n)),
            Err(e) => {
                warn!(error = %e, "discord_notifier_init_failed");
                None
            }
        },
        None => None,
    };

    let (trigger_tx, trigger_rx) = mpsc::channel::<()>(1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let app_state = AppState {
        cfg: cfg.clone(),
        cycle_state: cycle_state.clone(),
        trigger_tx,
        metrics,
        started_at: Instant::now(),
        trigger_window: Arc::new(Mutex::new(VecDeque::new())),
    };

    let api_for_boost = api_client.clone();
    let cfg_for_boost = cfg.clone();
    let cycle_state_for_boost = cycle_state.clone();
    let cooldown_for_boost = cooldown.clone();
    let notifier_for_boost = notifier.clone();
    let shutdown_rx_for_boost = shutdown_rx.clone();
    let boost_handle = tokio::spawn(async move {
        boost::run_cycle_loop(
            api_for_boost,
            cfg_for_boost,
            cycle_state_for_boost,
            cooldown_for_boost,
            notifier_for_boost,
            trigger_rx,
            shutdown_rx_for_boost,
        )
        .await;
    });

    let protected_routes = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/cycles", get(cycles_handler))
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            observability_auth,
        ));

    let app = Router::new()
        .route("/", get(health_root))
        .route("/healthz", get(health_handler))
        .route("/cycle", post(trigger_cycle))
        .merge(protected_routes)
        .with_state(app_state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind to {addr}"))?;
    info!(port = cfg.port, "http_server_listening");

    let shutdown_tx_for_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        warn!("shutdown_signal_received");
        let _ = shutdown_tx_for_signal.send(true);
    });

    let mut shutdown_for_axum = shutdown_rx.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_for_axum.changed().await;
    });

    if let Err(e) = server.await {
        warn!(error = ?e, "http_server_stopped");
    }

    info!("waiting_for_boost_loop_to_drain");
    let _ = tokio::time::timeout(cfg.cycle_deadline, boost_handle).await;

    info!("storebooster_exited");
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn health_root() -> &'static str {
    "alive\n"
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let cs = state.cycle_state.read().await;
    let now = Instant::now();

    let max_stale = state.cfg.boost_interval * 2 + state.cfg.cycle_deadline;
    let grace_period = state.cfg.boost_interval + state.cfg.cycle_deadline;

    let healthy = match cs.last_ended_at {
        Some(end) => now.duration_since(end) < max_stale,
        None => now.duration_since(state.started_at) < grace_period,
    };

    let body = json!({
        "status": if healthy { "healthy" } else { "stale" },
        "uptime_secs": now.duration_since(state.started_at).as_secs(),
        "last_cycle_started_secs_ago": cs.last_started_at.map(|t| now.duration_since(t).as_secs()),
        "last_cycle_ended_secs_ago": cs.last_ended_at.map(|t| now.duration_since(t).as_secs()),
        "last_summary": cs.history.back(),
    });

    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

async fn cycles_handler(State(state): State<AppState>) -> Json<Vec<CycleSummary>> {
    let cs = state.cycle_state.read().await;
    Json(cs.history.iter().cloned().collect())
}

/// Middleware for `/metrics` and `/cycles`. If `PUBLIC_OBSERVABILITY=1`,
/// pass through; otherwise require the same auth header pair as `/cycle`.
async fn observability_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    if state.cfg.public_observability {
        return next.run(request).await;
    }
    let got = headers
        .get(state.cfg.auth_header_name.as_str())
        .and_then(|h| h.to_str().ok());
    if got != Some(state.cfg.auth_header_value.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
}

async fn trigger_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, &'static str) {
    let got = headers
        .get(state.cfg.auth_header_name.as_str())
        .and_then(|h| h.to_str().ok());
    if got != Some(state.cfg.auth_header_value.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !rate_limit_admit(&state) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limited");
    }
    match state.trigger_tx.try_send(()) {
        Ok(()) => (StatusCode::ACCEPTED, "queued"),
        Err(mpsc::error::TrySendError::Full(_)) => {
            (StatusCode::TOO_MANY_REQUESTS, "cycle already pending")
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
        }
    }
}

/// Sliding-window rate limit for `/cycle`. Admits if fewer than
/// `cfg.trigger_rate_limit_per_min` triggers landed in the last 60s.
fn rate_limit_admit(state: &AppState) -> bool {
    let limit = state.cfg.trigger_rate_limit_per_min as usize;
    if limit == 0 {
        return true; // 0 disables the limit
    }
    let now = Instant::now();
    let mut win = state.trigger_window.lock().unwrap();
    while let Some(&front) = win.front() {
        if now.duration_since(front) > TRIGGER_RATE_WINDOW {
            win.pop_front();
        } else {
            break;
        }
    }
    if win.len() >= limit {
        return false;
    }
    win.push_back(now);
    true
}
