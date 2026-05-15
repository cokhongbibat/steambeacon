# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- Build: `cargo build` (debug) / `cargo build --release` (deploy artifact)
- Run locally: `cargo run --release` (requires `.env` with `SS_PRIVATE_KEY_DB`, `API_ENDPOINT`, `API_AUTH_HEADER_NAME`, `API_AUTH_HEADER_VALUE`)
- Lint: `cargo clippy`
- Tests: `cargo test`
- Single test: `cargo test --package storebooster -- crypto::tests::test_decrypt_gcm_roundtrip --exact`
- Crypto-module tests only: `cargo test crypto::`

## Architecture

Single binary that serves two roles in one tokio runtime:

1. **HTTP server** (`src/main.rs`) — axum router serving `/` (liveness), `/healthz` (readiness with cycle freshness), `/metrics` (Prometheus), `/cycles` (last-N summaries), and `POST /cycle` (auth-gated manual trigger).
2. **Boost cycle** (`src/boost.rs`) — a `tokio::spawn`'d background task that wakes every `BOOST_INTERVAL_SECS + rand([0, JITTER])` or on demand via the trigger channel.

Both share `AppState { cfg, cycle_state, trigger_tx, metrics, started_at }`. The boost loop owns the `mpsc::Receiver<()>` for triggers and a `watch::Receiver<bool>` for shutdown. `main` joins on the boost task with a timeout equal to `cycle_deadline` so a graceful shutdown lets the current cycle drain.

### Configuration (`src/config.rs`)

All env loading goes through `AppConfig::from_env`. Required vars (`API_ENDPOINT`, `API_AUTH_HEADER_NAME`, `API_AUTH_HEADER_VALUE`) **fail-closed** — missing or empty means the process bails with a non-zero exit. Optional knobs:

- `PORT` (default 3000)
- `BOOST_INTERVAL_SECS` (default 1200 = 20 min)
- `BOOST_INTERVAL_JITTER_SECS` (default 60)
- `BOOST_CYCLE_DEADLINE_SECS` (default 900 = 15 min)
- `BOOST_APP_IDS` (comma-separated, default `730` = CS2)
- `DRY_RUN` (truthy values: `1`, `true`, `yes` — case-insensitive)
- `BOOST_RESULT_REPORT` (same truthy parsing as `DRY_RUN`; off by default — only flip on once the bot exposes `POST /boostResult`)

`config::require_env_first(&["A", "B"])` is the canonical pattern for "use A, fall back to B". `crypto::init_key` uses it for `SS_PRIVATE_KEY_DB` → `PRIVATE_KEY_DB`.

### Cycle state and metrics

`src/state.rs` holds `CycleState { last_started_at, last_ended_at, history: VecDeque<CycleSummary> }`, capped at 10. Read by `/healthz` (freshness check + last summary) and `/cycles` (full history). `boost::run_cycle` updates it at start/end.

`metrics-exporter-prometheus` is installed once at startup; the `PrometheusHandle` lives in `AppState` and is rendered by `/metrics`. Metric names follow `storebooster_*`:
- `storebooster_cycles_total{status="ok"|"deadline_exceeded"|"fetch_failed"}` (counter)
- `storebooster_boost_attempts_total{outcome=...}` (counter, labels match `BoostOutcome::label`)
- `storebooster_boost_attempt_duration_seconds` (histogram)
- `storebooster_cycle_duration_seconds` (histogram)

### Boost cycle flow (`boost::run_cycle`)

1. `ApiClient::fetch_random_store_accounts_with_token(20)` → `GET {API_ENDPOINT}/getRandomStoreMyAccountWithToken?limit=20`, expects `{ "result": [{ steamId, refreshToken }] }`. The `refreshToken` field is a sealed payload from the companion `discord-natri-bot` service.
2. The driver sleeps 200ms *between* spawns so the N-th account starts 200·(N-1) ms after the cycle begins — staggered without leaving N idle tokio tasks sleeping.
3. Per-account: `crypto::decrypt_data` → `SteamClient::log_on` (15s timeout) → `games_played(vec![730])` (CS2 app id) → poll `SteamEvent`s up to 30s for `CSGOEvent::Online` → `log_off`. The decrypted refresh token is wrapped in `Zeroizing` so our local copy is wiped on drop; the clone handed to `LogOnDetails` lives inside `steam-client-rs` and is out of our hands.
4. `join_all` under a cycle deadline (default 15 min, override with `BOOST_CYCLE_DEADLINE_SECS`); aggregate counts are emitted as a single JSON tracing event (`boost_cycle_end`).
5. If `BOOST_RESULT_REPORT` is truthy, each per-account outcome is reported via `POST {API_ENDPOINT}/boostResult` with `{ steamId, outcome, elapsedMs }` (`BoostReport` in `src/api_client.rs`). Reports are spawned as detached tasks and `join_all`'d under a 30s `REPORT_BATCH_TIMEOUT`; failures are logged but never fail the cycle.

There is **no retry** and **no persistent bad-account state**. Failed accounts get fresh refresh tokens from the bot's own cron jobs on the next cycle.

`BoostOutcome` labels (drive the `outcome=` metric label and the reported outcome string): `success`, `no_csgo_online`, `log_on_failed`, `decrypt_failed`, `timed_out`, `dry_run`, `other`, plus `panic` emitted only by the cycle aggregator when a per-account task panics.

### Crypto wire format

`src/crypto.rs` decrypts values produced by `discord-natri-bot`'s `crypto_db.rs` `encrypt_data` helper. It is wire-compatible with that producer and **must stay so**:

- GCM (`v2:` prefix): `v2:<nonce_hex>:<ciphertext_hex>:<tag_hex>` — AES-256-GCM, 12-byte nonce, 16-byte tag. `aes-gcm` expects the tag appended to the ciphertext, which is why `decrypt_gcm` concatenates `payload = ciphertext || tag` before calling `cipher.decrypt`.
- CBC (legacy, no prefix): `<iv_hex>:<ciphertext_hex>` — AES-256-CBC, PKCS7 padding.

The 32-byte key comes from `SS_PRIVATE_KEY_DB` (64 hex chars). `crypto::init_key()` is called from `main` *before* any other startup work and exits the process on failure — fail-closed so the cycle can't run with a misconfigured key. The key is cached in a `OnceLock` for the process lifetime.

`crypto::is_encrypted` is intentionally permissive (hex-only colon-separated form) because Steam session cookies contain non-hex characters and never collide.

### API client contract

`ApiClient` (in `src/api_client.rs`) talks to the discord-natri-bot HTTP API. Auth is a single arbitrary header pair (`API_AUTH_HEADER_NAME` / `API_AUTH_HEADER_VALUE`) — the bot uses these names too (`SS_API_AUTH_HEADER_NAME`/`VALUE` on its side), so the values must be kept in sync. The response envelope is `WrappedResult<T> { result: T }`; if the bot is ever changed to return bare arrays, this struct needs updating.

### Logging

`tracing_subscriber` with `json()` output, level from `RUST_LOG` (default `info`). Field names (`steam_id`, `outcome`, `elapsed_ms`, `attempted`/`succeeded`/`failed`/`timed_out`, event names like `boost_cycle_end`) are part of the observable contract — log aggregators downstream key off them.

## Deployment

Render web service. `cargo build --release` then `./target/release/storebooster`. Render injects `PORT`; everything else comes from the service's env config and must match the companion `discord-natri-bot` service exactly for `API_AUTH_HEADER_*` and `SS_PRIVATE_KEY_DB`.

## Project history note

This is a Rust port of an earlier Node.js implementation. Anything that looks like a JS-ism (e.g. envelope shapes, env var names) is intentional for wire/config compatibility with the bot, not a translation artifact to "clean up."
