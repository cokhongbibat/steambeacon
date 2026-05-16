use std::time::Duration;

use anyhow::Context;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::warn;

pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    auth_header_name: String,
    auth_header_value: String,
}

#[derive(Debug, Deserialize)]
pub struct AccountWithToken {
    #[serde(rename = "steamId")]
    pub steam_id: String,
    /// Sealed refresh token (`v2:nonce_hex:ct_hex:tag_hex`)
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct WrappedResult<T> {
    result: T,
}

#[derive(Debug, Serialize)]
pub struct BoostReport {
    #[serde(rename = "steamId")]
    pub steam_id: String,
    pub outcome: String,
    #[serde(rename = "elapsedMs")]
    pub elapsed_ms: u64,
}

impl ApiClient {
    pub fn new(base_url: String, auth_name: String, auth_value: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            http,
            base_url,
            auth_header_name: auth_name,
            auth_header_value: auth_value,
        })
    }

    pub async fn fetch_random_store_accounts_with_token(
        &self,
        limit: usize,
        max_retries: u32,
    ) -> anyhow::Result<Vec<AccountWithToken>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let backoff = backoff_for(attempt);
                warn!(
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "fetch_accounts_retry"
                );
                tokio::time::sleep(backoff).await;
            }
            match self.fetch_once(limit).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry exhausted")))
    }

    async fn fetch_once(&self, limit: usize) -> anyhow::Result<Vec<AccountWithToken>> {
        let url = format!(
            "{}/getRandomStoreMyAccountWithToken?limit={}",
            self.base_url, limit
        );

        let response = self
            .http
            .get(&url)
            .header(&self.auth_header_name, &self.auth_header_value)
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_owned());
            let excerpt = body.chars().take(200).collect::<String>();
            anyhow::bail!("API returned {status}: {excerpt}");
        }

        let envelope: WrappedResult<Vec<AccountWithToken>> = response
            .json()
            .await
            .context("failed to parse API JSON response")?;

        Ok(envelope.result)
    }

    /// Fire-and-forget style: posts a single boost outcome. Returns `Err` on
    /// transport or non-2xx so the caller can log; cycle correctness does not
    /// depend on this call succeeding.
    pub async fn report_boost_result(&self, report: BoostReport) -> anyhow::Result<()> {
        let url = format!("{}/boostResult", self.base_url);
        let response = self
            .http
            .post(&url)
            .header(&self.auth_header_name, &self.auth_header_value)
            .json(&report)
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_owned());
            let excerpt = body.chars().take(200).collect::<String>();
            anyhow::bail!("boostResult returned {status}: {excerpt}");
        }
        Ok(())
    }
}

/// Exponential backoff with jitter for `attempt >= 1`.
/// 1→500ms, 2→1s, 3→2s, 4→4s, capped at 8s, plus 0–200ms jitter.
fn backoff_for(attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(4);
    let base_ms: u64 = 500u64.saturating_mul(1u64 << exp);
    let jitter_ms: u64 = rand::thread_rng().gen_range(0..200);
    Duration::from_millis(base_ms.min(8_000) + jitter_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        let a1 = backoff_for(1).as_millis();
        let a2 = backoff_for(2).as_millis();
        let a3 = backoff_for(3).as_millis();
        // monotonic up to cap; allow jitter slack of 200ms
        assert!(a1 < a2 + 200);
        assert!(a2 < a3 + 200);
        // capped at 8s + 200ms jitter
        assert!(backoff_for(99).as_millis() <= 8_200);
    }
}
