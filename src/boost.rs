use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use futures::future::join_all;
use metrics::{counter, histogram};
use rand::Rng;
use steam_client::{
    CSGOEvent, ConnectionEvent, LogOnDetails, SteamClient, SteamEvent, SteamOptions,
};
use tokio::sync::{mpsc, watch, RwLock};
use tokio::time::sleep;
use tracing::{error, info, info_span, warn, Instrument};
use zeroize::Zeroizing;

use crate::api_client::{ApiClient, BoostReport};
use crate::config::AppConfig;
use crate::cooldown::CooldownTracker;
use crate::crypto;
use crate::notifier::DiscordNotifier;
use crate::state::{CycleState, CycleSummary};

const STAGGER: Duration = Duration::from_millis(200);
const LOG_ON_TIMEOUT: Duration = Duration::from_secs(15);
const ONLINE_WAIT: Duration = Duration::from_secs(30);
const STARTUP_DELAY: Duration = Duration::from_secs(5);
const REPORT_BATCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
#[allow(dead_code)] // string payloads read via Debug in tracing logs
enum BoostOutcome {
    Success,
    NoCsgoOnline,
    LogOnFailed(String),
    InvalidToken(String),
    DecryptFailed(String),
    TimedOut,
    DryRun,
    Skipped,
    Other(String),
}

impl BoostOutcome {
    fn label(&self) -> &'static str {
        match self {
            BoostOutcome::Success => "success",
            BoostOutcome::NoCsgoOnline => "no_csgo_online",
            BoostOutcome::LogOnFailed(_) => "log_on_failed",
            BoostOutcome::InvalidToken(_) => "invalid_token",
            BoostOutcome::DecryptFailed(_) => "decrypt_failed",
            BoostOutcome::TimedOut => "timed_out",
            BoostOutcome::DryRun => "dry_run",
            BoostOutcome::Skipped => "skipped",
            BoostOutcome::Other(_) => "other",
        }
    }
}

/// Returns `true` if the steam-client-rs log-on error string indicates the
/// refresh token is unusable (revoked, expired, wrong format, etc.) — as
/// opposed to a transient network or CM-availability issue.
fn looks_like_token_problem(err_text: &str) -> bool {
    let lower = err_text.to_ascii_lowercase();
    [
        "invalidpassword",
        "invalidrefreshtoken",
        "invalidtoken",
        "accessdenied",
        "expired",
        "revoked",
        "twofactorrequired",
        "accountlogondenied",
        "accountdisabled",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Boost loop entry point. Selects across the periodic timer, manual triggers,
/// and the shutdown channel. Shutdown during sleep cancels immediately; shutdown
/// during a cycle lets the current cycle finish (up to `cfg.cycle_deadline`).
pub async fn run_cycle_loop(
    api: Arc<ApiClient>,
    cfg: Arc<AppConfig>,
    cycle_state: Arc<RwLock<CycleState>>,
    cooldown: Arc<CooldownTracker>,
    notifier: Option<Arc<DiscordNotifier>>,
    mut trigger_rx: mpsc::Receiver<()>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    sleep(STARTUP_DELAY).await;
    info!(
        cron = cfg.cron_schedule.is_some(),
        interval_secs = cfg.boost_interval.as_secs(),
        "boost_cycle_loop_started"
    );

    loop {
        let sleep_for = next_sleep(&cfg);

        let should_run = tokio::select! {
            _ = sleep(sleep_for) => true,
            Some(_) = trigger_rx.recv() => {
                info!("boost_cycle_triggered_manual");
                true
            }
            res = shutdown_rx.changed() => {
                if res.is_ok() && *shutdown_rx.borrow() {
                    info!("boost_cycle_loop_shutdown_during_sleep");
                    return;
                }
                false
            }
        };

        if !should_run {
            continue;
        }

        run_cycle(&api, &cfg, &cycle_state, &cooldown, notifier.as_deref()).await;

        if *shutdown_rx.borrow() {
            info!("boost_cycle_loop_shutdown_after_cycle");
            return;
        }
    }
}

/// Pick the next sleep duration. Cron takes precedence; if no cron expression
/// is configured (or it has no upcoming firing), fall back to interval+jitter.
fn next_sleep(cfg: &AppConfig) -> Duration {
    if let Some(schedule) = &cfg.cron_schedule {
        if let Some(next) = schedule.upcoming(Utc).next() {
            let delta = next.signed_duration_since(Utc::now());
            return delta.to_std().unwrap_or_else(|_| Duration::from_secs(1));
        }
    }
    cfg.boost_interval + pick_jitter(cfg.interval_jitter)
}

fn pick_jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let max_ms = max.as_millis() as u64;
    Duration::from_millis(rand::thread_rng().gen_range(0..=max_ms))
}

async fn run_cycle(
    api: &Arc<ApiClient>,
    cfg: &AppConfig,
    cycle_state: &RwLock<CycleState>,
    cooldown: &CooldownTracker,
    notifier: Option<&DiscordNotifier>,
) {
    let cycle_start = Instant::now();
    let cycle_start_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    cycle_state.write().await.record_start();

    let accounts = match api
        .fetch_random_store_accounts_with_token(cfg.accounts_per_cycle, cfg.bot_api_max_retries)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            error!(error = %e, "boost_cycle_fetch_failed");
            counter!("storebooster_cycles_total", "status" => "fetch_failed").increment(1);
            let summary = CycleSummary {
                started_unix_secs: cycle_start_unix,
                duration_ms: cycle_start.elapsed().as_millis() as u64,
                total: 0,
                succeeded: 0,
                failed: 0,
                timed_out: 0,
                no_csgo_online: 0,
                decrypt_failed: 0,
                invalid_token: 0,
                skipped: 0,
                deadline_exceeded: false,
            };
            cycle_state.write().await.record_end(summary.clone());
            if let Some(n) = notifier {
                n.notify_cycle_alert(&summary, "bot API fetch failed").await;
            }
            return;
        }
    };

    let total = accounts.len();
    let app_ids_label = app_ids_label(&cfg.app_ids);
    info!(
        total,
        dry_run = cfg.dry_run,
        cooling = cooldown.cooling_count(),
        "boost_cycle_start"
    );

    let app_ids = Arc::new(cfg.app_ids.clone());
    let dry_run = cfg.dry_run;

    let mut tasks = Vec::with_capacity(total);
    let mut skipped = 0usize;
    for (idx, account) in accounts.into_iter().enumerate() {
        let steam_id = account.steam_id.clone();
        if cooldown.should_skip(&steam_id) {
            info!(steam_id = %steam_id, "boost_account_skipped_cooldown");
            counter!(
                "storebooster_boost_attempts_total",
                "outcome" => "skipped",
                "apps" => app_ids_label.clone(),
            )
            .increment(1);
            skipped += 1;
            continue;
        }
        if idx > 0 {
            sleep(STAGGER).await;
        }
        let app_ids = app_ids.clone();
        let refresh = account.refresh_token;
        tasks.push(tokio::spawn(async move {
            let start = Instant::now();
            let outcome = boost_account(&steam_id, &refresh, &app_ids, dry_run)
                .instrument(info_span!("boost_account", steam_id = %steam_id))
                .await;
            (steam_id, outcome, start.elapsed())
        }));
    }

    let (results, deadline_exceeded) =
        match tokio::time::timeout(cfg.cycle_deadline, join_all(tasks)).await {
            Ok(r) => (r, false),
            Err(_) => {
                warn!(total, "boost_cycle_deadline_exceeded");
                (vec![], true)
            }
        };

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut timed_out = 0usize;
    let mut no_csgo_online = 0usize;
    let mut decrypt_failed = 0usize;
    let mut invalid_token = 0usize;
    let mut report_tasks = Vec::new();

    for jr in results {
        match jr {
            Ok((steam_id, outcome, elapsed)) => {
                let label = outcome.label();
                let is_failure = matches!(
                    outcome,
                    BoostOutcome::NoCsgoOnline
                        | BoostOutcome::LogOnFailed(_)
                        | BoostOutcome::InvalidToken(_)
                        | BoostOutcome::DecryptFailed(_)
                        | BoostOutcome::TimedOut
                        | BoostOutcome::Other(_)
                );
                match &outcome {
                    BoostOutcome::Success | BoostOutcome::DryRun => succeeded += 1,
                    BoostOutcome::NoCsgoOnline => no_csgo_online += 1,
                    BoostOutcome::TimedOut => timed_out += 1,
                    BoostOutcome::DecryptFailed(_) => decrypt_failed += 1,
                    BoostOutcome::InvalidToken(_) => invalid_token += 1,
                    BoostOutcome::Skipped => { /* counted at dispatch */ }
                    _ => failed += 1,
                }
                if matches!(outcome, BoostOutcome::Success) {
                    cooldown.record_success(&steam_id);
                } else if is_failure {
                    cooldown.record_failure(&steam_id);
                }

                counter!(
                    "storebooster_boost_attempts_total",
                    "outcome" => label,
                    "apps" => app_ids_label.clone(),
                )
                .increment(1);
                histogram!("storebooster_boost_attempt_duration_seconds")
                    .record(elapsed.as_secs_f64());

                if cfg.report_results && !matches!(outcome, BoostOutcome::Skipped) {
                    let api = api.clone();
                    let report = BoostReport {
                        steam_id: steam_id.clone(),
                        outcome: label.to_owned(),
                        elapsed_ms: elapsed.as_millis() as u64,
                    };
                    report_tasks.push(tokio::spawn(async move {
                        let steam_id = report.steam_id.clone();
                        if let Err(e) = api.report_boost_result(report).await {
                            warn!(steam_id = %steam_id, error = %e, "boost_result_report_failed");
                        }
                    }));
                }
            }
            Err(e) => {
                error!(error = %e, "boost_task_panicked");
                failed += 1;
                counter!(
                    "storebooster_boost_attempts_total",
                    "outcome" => "panic",
                    "apps" => app_ids_label.clone(),
                )
                .increment(1);
            }
        }
    }

    if !report_tasks.is_empty() {
        let _ = tokio::time::timeout(REPORT_BATCH_TIMEOUT, join_all(report_tasks)).await;
    }

    let cycle_status_label = if deadline_exceeded {
        "deadline_exceeded"
    } else {
        "ok"
    };
    counter!("storebooster_cycles_total", "status" => cycle_status_label).increment(1);
    histogram!("storebooster_cycle_duration_seconds").record(cycle_start.elapsed().as_secs_f64());

    let summary = CycleSummary {
        started_unix_secs: cycle_start_unix,
        duration_ms: cycle_start.elapsed().as_millis() as u64,
        total,
        succeeded,
        failed,
        timed_out,
        no_csgo_online,
        decrypt_failed,
        invalid_token,
        skipped,
        deadline_exceeded,
    };

    info!(
        succeeded,
        failed,
        timed_out,
        no_csgo_online,
        decrypt_failed,
        invalid_token,
        skipped,
        deadline_exceeded,
        duration_ms = summary.duration_ms,
        "boost_cycle_end"
    );

    cycle_state.write().await.record_end(summary.clone());

    if let Some(n) = notifier {
        let denom = total.saturating_sub(skipped) as f64;
        let ratio = if denom > 0.0 {
            succeeded as f64 / denom
        } else {
            1.0
        };
        if deadline_exceeded {
            n.notify_cycle_alert(&summary, "deadline exceeded").await;
        } else if ratio < cfg.alert_threshold_ratio {
            let reason = format!(
                "success ratio {:.2} below threshold {:.2}",
                ratio, cfg.alert_threshold_ratio
            );
            n.notify_cycle_alert(&summary, &reason).await;
        }
    }
}

fn app_ids_label(app_ids: &[u32]) -> String {
    let mut copy: Vec<u32> = app_ids.to_vec();
    copy.sort_unstable();
    copy.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

async fn boost_account(
    steam_id: &str,
    refresh_token_sealed: &str,
    app_ids: &[u32],
    dry_run: bool,
) -> BoostOutcome {
    let _ = steam_id; // span carries it; kept for explicit signature
    let plaintext = match crypto::decrypt_data(refresh_token_sealed) {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "refresh_token_decrypt_failed");
            return BoostOutcome::DecryptFailed(e.to_string());
        }
    };
    let plaintext = Zeroizing::new(plaintext);

    if dry_run {
        info!("dry_run_skip");
        return BoostOutcome::DryRun;
    }

    let mut options = SteamOptions::default();
    options.reconnect.enabled = false;
    let mut client = SteamClient::new(options);

    let log_on_details = LogOnDetails {
        refresh_token: Some((*plaintext).clone()),
        ..Default::default()
    };

    match tokio::time::timeout(LOG_ON_TIMEOUT, client.log_on(log_on_details)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let text = format!("{e:?}");
            if looks_like_token_problem(&text) {
                warn!(error = %text, "log_on_invalid_token");
                return BoostOutcome::InvalidToken(text);
            }
            warn!(error = %text, "log_on_failed");
            return BoostOutcome::LogOnFailed(text);
        }
        Err(_) => {
            warn!("log_on_timeout");
            return BoostOutcome::TimedOut;
        }
    }

    if let Err(e) = client.games_played(app_ids.to_vec()).await {
        warn!(error = ?e, "games_played_failed");
        let _ = client.log_off().await;
        return BoostOutcome::Other(format!("games_played: {e:?}"));
    }

    let deadline = Instant::now() + ONLINE_WAIT;
    let mut online = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match client.poll_event_timeout(remaining).await {
            Ok(Some(SteamEvent::CSGO(CSGOEvent::Online(_)))) => {
                online = true;
                break;
            }
            Ok(Some(SteamEvent::Connection(ConnectionEvent::Disconnected {
                will_reconnect: false,
                ..
            }))) => {
                warn!("disconnected_during_gc_wait");
                break;
            }
            Ok(Some(_)) | Ok(None) => continue,
            Err(e) => {
                warn!(error = ?e, "poll_event_error");
                break;
            }
        }
    }

    let _ = client.log_off().await;

    if online {
        BoostOutcome::Success
    } else {
        BoostOutcome::NoCsgoOnline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_problem_detection() {
        assert!(looks_like_token_problem("InvalidPassword"));
        assert!(looks_like_token_problem("Auth(AccessDenied)"));
        assert!(looks_like_token_problem("token has been revoked"));
        assert!(looks_like_token_problem("InvalidRefreshToken"));
        assert!(!looks_like_token_problem("ConnectionTimeout"));
        assert!(!looks_like_token_problem("ServiceUnavailable"));
    }

    #[test]
    fn app_ids_label_is_sorted_joined() {
        assert_eq!(app_ids_label(&[730]), "730");
        assert_eq!(app_ids_label(&[570, 730, 440]), "440,570,730");
        assert_eq!(app_ids_label(&[]), "");
    }
}
