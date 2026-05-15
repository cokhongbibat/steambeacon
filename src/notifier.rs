//! Discord webhook notifier for cycle-level alerts.
//!
//! Optional — only constructed if `DISCORD_WEBHOOK_URL` is set. The boost
//! cycle calls `notify_cycle_alert` after every cycle that crosses the
//! degradation threshold (deadline exceeded, or success ratio below
//! `BOOST_ALERT_THRESHOLD_RATIO`).
//!
//! Failures are logged at `warn` and never propagated — alerting must
//! never fail the cycle.

use std::time::Duration;

use anyhow::Context;
use serde_json::json;
use tracing::warn;

use crate::state::CycleSummary;

const DISCORD_RED: u32 = 0xE74C3C;

pub struct DiscordNotifier {
    http: reqwest::Client,
    webhook_url: String,
}

impl DiscordNotifier {
    pub fn new(webhook_url: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .context("failed to build discord http client")?;
        Ok(Self { http, webhook_url })
    }

    pub async fn notify_cycle_alert(&self, summary: &CycleSummary, reason: &str) {
        let title = format!("storebooster: cycle degraded — {reason}");
        let body = format!(
            "total={}\nsucceeded={}\nfailed={}\ntimed_out={}\nno_csgo_online={}\ndecrypt_failed={}\ninvalid_token={}\nskipped={}\ndeadline_exceeded={}\nduration_ms={}",
            summary.total,
            summary.succeeded,
            summary.failed,
            summary.timed_out,
            summary.no_csgo_online,
            summary.decrypt_failed,
            summary.invalid_token,
            summary.skipped,
            summary.deadline_exceeded,
            summary.duration_ms,
        );
        let payload = json!({
            "embeds": [{
                "title": title,
                "description": format!("```\n{body}\n```"),
                "color": DISCORD_RED,
            }]
        });

        if let Err(e) = self.post(payload).await {
            warn!(error = %e, "discord_notify_failed");
        }
    }

    async fn post(&self, payload: serde_json::Value) -> anyhow::Result<()> {
        let response = self
            .http
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
            .context("discord webhook POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            let excerpt = body.chars().take(200).collect::<String>();
            anyhow::bail!("discord webhook returned {status}: {excerpt}");
        }
        Ok(())
    }
}
