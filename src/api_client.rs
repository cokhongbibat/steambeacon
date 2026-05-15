use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

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
    ) -> anyhow::Result<Vec<AccountWithToken>> {
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
