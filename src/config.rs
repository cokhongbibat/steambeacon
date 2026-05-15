use std::time::Duration;

use anyhow::Context;

/// Look up the first non-empty env var by name. Returns the matched name and value.
/// Errors if every candidate is unset or empty.
pub fn require_env_first<'a>(names: &'a [&'a str]) -> anyhow::Result<(&'a str, String)> {
    for &name in names {
        if let Ok(v) = std::env::var(name) {
            if !v.is_empty() {
                return Ok((name, v));
            }
        }
    }
    anyhow::bail!(
        "Missing required environment variable: {}",
        names.join(" / ")
    )
}

fn duration_secs_or(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

fn bool_env(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True") | Ok("yes") | Ok("YES")
    )
}

fn parse_app_ids(raw: &str) -> anyhow::Result<Vec<u32>> {
    if raw.trim().is_empty() {
        return Ok(vec![730]);
    }
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u32>()
                .with_context(|| format!("BOOST_APP_IDS: invalid app id {s:?}"))
        })
        .collect()
}

pub struct AppConfig {
    pub port: u16,
    pub api_endpoint: String,
    pub auth_header_name: String,
    pub auth_header_value: String,
    pub boost_interval: Duration,
    pub cycle_deadline: Duration,
    pub interval_jitter: Duration,
    pub app_ids: Vec<u32>,
    pub dry_run: bool,
    pub report_results: bool,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);

        let (_, api_endpoint) = require_env_first(&["API_ENDPOINT"])?;
        let (_, auth_header_name) = require_env_first(&["API_AUTH_HEADER_NAME"])?;
        let (_, auth_header_value) = require_env_first(&["API_AUTH_HEADER_VALUE"])?;

        let app_ids = parse_app_ids(&std::env::var("BOOST_APP_IDS").unwrap_or_default())?;
        let dry_run = bool_env("DRY_RUN");
        let report_results = bool_env("BOOST_RESULT_REPORT");

        Ok(Self {
            port,
            api_endpoint,
            auth_header_name,
            auth_header_value,
            boost_interval: duration_secs_or("BOOST_INTERVAL_SECS", Duration::from_secs(20 * 60)),
            cycle_deadline: duration_secs_or(
                "BOOST_CYCLE_DEADLINE_SECS",
                Duration::from_secs(15 * 60),
            ),
            interval_jitter: duration_secs_or(
                "BOOST_INTERVAL_JITTER_SECS",
                Duration::from_secs(60),
            ),
            app_ids,
            dry_run,
            report_results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_empty_defaults_to_cs2() {
        assert_eq!(parse_app_ids("").unwrap(), vec![730]);
        assert_eq!(parse_app_ids("   ").unwrap(), vec![730]);
    }

    #[test]
    fn app_ids_parses_single_and_csv() {
        assert_eq!(parse_app_ids("570").unwrap(), vec![570]);
        assert_eq!(parse_app_ids("730, 570 ,440").unwrap(), vec![730, 570, 440]);
    }

    #[test]
    fn app_ids_skips_empty_segments() {
        assert_eq!(parse_app_ids("730,,570,").unwrap(), vec![730, 570]);
    }

    #[test]
    fn app_ids_errors_on_garbage() {
        assert!(parse_app_ids("abc").is_err());
        assert!(parse_app_ids("730,abc,570").is_err());
        assert!(parse_app_ids("-1").is_err()); // u32 rejects negatives
    }

    #[test]
    fn duration_secs_or_falls_back_when_unset() {
        // Use a name no test would set
        let key = "STOREBOOSTER_TEST_UNSET_KEY_XYZ";
        std::env::remove_var(key);
        assert_eq!(
            duration_secs_or(key, Duration::from_secs(42)),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn duration_secs_or_parses_when_set() {
        let key = "STOREBOOSTER_TEST_DURATION_KEY";
        std::env::set_var(key, "7");
        assert_eq!(
            duration_secs_or(key, Duration::from_secs(99)),
            Duration::from_secs(7)
        );
        std::env::remove_var(key);
    }

    #[test]
    fn duration_secs_or_falls_back_on_garbage() {
        let key = "STOREBOOSTER_TEST_DURATION_GARBAGE";
        std::env::set_var(key, "not_a_number");
        assert_eq!(
            duration_secs_or(key, Duration::from_secs(11)),
            Duration::from_secs(11)
        );
        std::env::remove_var(key);
    }

    #[test]
    fn bool_env_accepts_truthy_forms() {
        for v in ["1", "true", "TRUE", "True", "yes", "YES"] {
            let key = format!("STOREBOOSTER_TEST_BOOL_{v}");
            std::env::set_var(&key, v);
            assert!(bool_env(&key), "expected truthy for {v}");
            std::env::remove_var(&key);
        }
    }

    #[test]
    fn bool_env_rejects_other_values() {
        for v in ["0", "false", "no", "", "anything"] {
            let key = format!("STOREBOOSTER_TEST_BOOL_FALSE_{}", v.len());
            std::env::set_var(&key, v);
            assert!(!bool_env(&key), "expected falsy for {v:?}");
            std::env::remove_var(&key);
        }
    }
}
