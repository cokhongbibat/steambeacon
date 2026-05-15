# storebooster (Rust)

Periodic Steam "store boost" task. Polls the discord-natri-bot HTTP API every
20 minutes (configurable) for store-tagged Steam accounts, decrypts their
refresh tokens, logs into Steam CM via `steam-client-rs`, marks each account
as "playing CS2" (app id 730) for a brief window, then logs off.

Replaces the previous Node.js implementation. Same Render deployment shape:
binds `PORT`, runs the boost cycle as a tokio task in the same process.

## Build & Run (local)

    cp .env.example .env   # if you have one; otherwise create it
    # Fill in API_ENDPOINT, API_AUTH_HEADER_*, SS_PRIVATE_KEY_DB
    cargo run --release

## Env Vars

All required vars are **fail-closed**: missing or empty exits non-zero on
startup rather than running broken.

| Var | Required | Default | Purpose |
|---|---|---|---|
| `PORT` | no | `3000` | Render-provided; axum binds here |
| `API_ENDPOINT` | **yes** | — | discord-natri-bot HTTP API base URL |
| `API_AUTH_HEADER_NAME` | **yes** | — | Auth header name (e.g. `auth-x-secret`) |
| `API_AUTH_HEADER_VALUE` | **yes** | — | Auth header value (also gates `POST /cycle`) |
| `SS_PRIVATE_KEY_DB` | **yes** *(or `PRIVATE_KEY_DB`)* | — | 32-byte hex (64 chars) AES-256 key for refresh-token decrypt |
| `PRIVATE_KEY_DB` | fallback | — | Used only if `SS_PRIVATE_KEY_DB` is unset/empty |
| `BOOST_INTERVAL_SECS` | no | `1200` (20 min) | Cycle wake interval |
| `BOOST_INTERVAL_JITTER_SECS` | no | `60` | Random jitter `[0, jitter]` added to each tick |
| `BOOST_CYCLE_DEADLINE_SECS` | no | `900` (15 min) | Per-cycle hard deadline |
| `BOOST_APP_IDS` | no | `730` | Comma-separated Steam app ids passed to `games_played` |
| `BOOST_ACCOUNTS_PER_CYCLE` | no | `20` | Accounts requested per cycle (`?limit=` on bot fetch) |
| `BOOST_CRON` | no | — | Cron expression (6-field with seconds, e.g. `0 */5 * * * *`). If set, overrides `BOOST_INTERVAL_SECS` for next-wake; deadline + jitter still apply. |
| `BOOST_COOLDOWN_THRESHOLD` | no | `3` | Consecutive per-account failures before that steam_id is benched. `0` disables. |
| `BOOST_COOLDOWN_CYCLES` | no | `2` | Number of cycles to bench an account after it hits the threshold. |
| `BOT_API_MAX_RETRIES` | no | `2` | Retries for `getRandomStoreMyAccountWithToken` with exp backoff (500ms→1s→2s→4s→8s, +jitter). |
| `DISCORD_WEBHOOK_URL` | no | — | If set, post a Discord embed when a cycle is degraded (deadline exceeded, or success ratio below threshold). |
| `BOOST_ALERT_THRESHOLD_RATIO` | no | `0.5` | Success-ratio threshold for alerting (`succeeded / (total - skipped)`). |
| `PUBLIC_OBSERVABILITY` | no | `false` | If `1`, `/metrics` + `/cycles` unauthed. Default requires the same auth header as `/cycle`. |
| `TRIGGER_RATE_LIMIT_PER_MIN` | no | `6` | Sliding-window rate limit on `POST /cycle`. `0` disables. |
| `DRY_RUN` | no | `false` | If `1`/`true`, decrypt only — never connect to Steam |
| `BOOST_RESULT_REPORT` | no | `false` | If truthy, `POST {API_ENDPOINT}/boostResult` per account after each attempt. Leave off unless the bot exposes that route. |
| `RUST_LOG` | no | `info` | tracing-subscriber `EnvFilter` directive |

`API_AUTH_HEADER_NAME`/`VALUE` and the private key must match the values
configured on the companion `discord-natri-bot` service (`SS_API_AUTH_HEADER_*`
and `SS_PRIVATE_KEY_DB` on its side).

## HTTP Endpoints

| Method + Path | Auth | Purpose |
|---|---|---|
| `GET /` | none | Liveness — returns `alive` |
| `GET /healthz` | none | Readiness — `503` if no cycle finished within `2 × interval + deadline`. JSON body includes uptime, seconds-since-last-cycle, and the last cycle summary. |
| `GET /metrics` | `API_AUTH_HEADER_*` *(unauthed if `PUBLIC_OBSERVABILITY=1`)* | Prometheus exposition: `storebooster_cycles_total{status}`, `storebooster_boost_attempts_total{outcome,apps}`, `_boost_attempt_duration_seconds`, `_cycle_duration_seconds`. |
| `GET /cycles` | `API_AUTH_HEADER_*` *(unauthed if `PUBLIC_OBSERVABILITY=1`)* | Last 10 cycle summaries as JSON. |
| `POST /cycle` | `API_AUTH_HEADER_*` | Trigger a cycle now. `202` queued, `429` if one is already pending or rate-limited (`TRIGGER_RATE_LIMIT_PER_MIN`), `401` on bad auth. |

## Boost Cycle

Every `BOOST_INTERVAL_SECS` + jitter (or per `BOOST_CRON` if set), or on `POST /cycle`:
1. `GET {API_ENDPOINT}/getRandomStoreMyAccountWithToken?limit={BOOST_ACCOUNTS_PER_CYCLE}`
   with up to `BOT_API_MAX_RETRIES` retries on transient failure (exponential
   backoff + jitter).
2. For each account, check the in-process cooldown tracker — if the account is
   currently benched (≥ `BOOST_COOLDOWN_THRESHOLD` consecutive prior failures),
   it is skipped and counted under `outcome=skipped`. Otherwise spawn a tokio
   task; the driver sleeps 200 ms between spawns so the N-th account starts
   200·(N-1) ms in (stagger without idle tasks).
   - decrypt sealed `refreshToken`
   - `SteamClient::log_on(LogOnDetails { refresh_token })` with 15s timeout
   - `games_played(BOOST_APP_IDS)`
   - Wait up to 30s for `SteamEvent::CSGO(CSGOEvent::Online)`
   - `log_off()`
3. `BOOST_CYCLE_DEADLINE_SECS` cap on the whole cycle; aggregate counts logged
   as one JSON event (`boost_cycle_end`). Outcomes classify into `success`,
   `no_csgo_online`, `log_on_failed`, `invalid_token` (revoked/expired token),
   `timed_out`, `decrypt_failed`, `dry_run`, `skipped`, `other`.
4. Per account, fire `POST {API_ENDPOINT}/boostResult` with
   `{ steamId, outcome, elapsedMs }`. Reporting failures are logged but do not
   fail the cycle. Skipped accounts are not reported.
5. If `DISCORD_WEBHOOK_URL` is set and the cycle is degraded (deadline exceeded
   or success ratio below `BOOST_ALERT_THRESHOLD_RATIO`), an embed is posted to
   the webhook. Notification failures are logged-only.

If `DRY_RUN=1`, accounts are fetched and refresh tokens decrypted, but the
Steam connection step is skipped — useful for smoke-testing config without
touching CMs.

## Shutdown

`SIGTERM` (Render's stop signal) and `ctrl_c` trigger graceful shutdown:
- HTTP server stops accepting new connections; in-flight requests finish.
- If the boost loop is mid-cycle, the cycle runs to completion (capped by
  `BOOST_CYCLE_DEADLINE_SECS`) before exit; if it's sleeping, it exits
  immediately.

## Errors

Skip + log; no per-cycle retry, no persistent bad-account state. The bot's
cronjob renews refresh tokens (`job_refresh_refresh_token` weekly,
`job_refresh_access_token` every 30 min), so a token failure today gets a
fresh token tomorrow.

## Deployment

Render web service. Native Rust runtime is preferred:
- Build command: `cargo build --release`
- Run command: `./target/release/storebooster`

The boost cycle is a tokio task in the same process; no separate worker
service required.

### Render env checklist

| Var | Value to set |
|---|---|
| `API_ENDPOINT` | URL of the companion `discord-natri-bot` service |
| `API_AUTH_HEADER_NAME` | Same string as bot's `SS_API_AUTH_HEADER_NAME` |
| `API_AUTH_HEADER_VALUE` | Same string as bot's `SS_API_AUTH_HEADER_VALUE` |
| `SS_PRIVATE_KEY_DB` | Same 64-hex string as bot's `SS_PRIVATE_KEY_DB` |
| `RUST_LOG` | `info` (or `info,storebooster=debug` while debugging) |

Leave defaults for `PORT` (Render injects), `BOOST_INTERVAL_SECS`,
`BOOST_INTERVAL_JITTER_SECS`, `BOOST_CYCLE_DEADLINE_SECS`, `BOOST_APP_IDS`,
`DRY_RUN`, `BOOST_RESULT_REPORT`.

Set `DRY_RUN=1` for the first deploy if you want to verify the binary boots
and reaches the bot API before touching Steam CMs; flip it back off once
`/healthz` and `/cycles` look healthy.

Only set `BOOST_RESULT_REPORT=1` *after* the bot has the matching
`POST /boostResult` route — otherwise every cycle will log ~20 warnings
per cycle (cycle correctness is unaffected, but the noise is real).
