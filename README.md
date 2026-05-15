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
| `DRY_RUN` | no | `false` | If `1`/`true`, decrypt only — never connect to Steam |
| `RUST_LOG` | no | `info` | tracing-subscriber `EnvFilter` directive |

`API_AUTH_HEADER_NAME`/`VALUE` and the private key must match the values
configured on the companion `discord-natri-bot` service (`SS_API_AUTH_HEADER_*`
and `SS_PRIVATE_KEY_DB` on its side).

## HTTP Endpoints

| Method + Path | Auth | Purpose |
|---|---|---|
| `GET /` | none | Liveness — returns `alive` |
| `GET /healthz` | none | Readiness — `503` if no cycle finished within `2 × interval + deadline`. JSON body includes uptime, seconds-since-last-cycle, and the last cycle summary. |
| `GET /metrics` | none | Prometheus exposition: `storebooster_cycles_total{status}`, `storebooster_boost_attempts_total{outcome}`, `_boost_attempt_duration_seconds`, `_cycle_duration_seconds`. |
| `GET /cycles` | none | Last 10 cycle summaries as JSON. |
| `POST /cycle` | `API_AUTH_HEADER_*` | Trigger a cycle now. `202` queued, `429` if one is already pending, `401` on bad auth. |

## Boost Cycle

Every `BOOST_INTERVAL_SECS` + jitter, or on `POST /cycle`:
1. `GET {API_ENDPOINT}/getRandomStoreMyAccountWithToken?limit=20`
2. For each account, spawn a tokio task; the driver sleeps 200 ms between
   spawns so the N-th account starts 200·(N-1) ms in (stagger without idle
   tasks).
   - decrypt sealed `refreshToken`
   - `SteamClient::log_on(LogOnDetails { refresh_token })` with 15s timeout
   - `games_played(vec![730])`
   - Wait up to 30s for `SteamEvent::CSGO(CSGOEvent::Online)`
   - `log_off()`
3. `BOOST_CYCLE_DEADLINE_SECS` cap on the whole cycle; aggregate counts logged
   as one JSON event (`boost_cycle_end`).
4. Per account, fire `POST {API_ENDPOINT}/boostResult` with
   `{ steamId, outcome, elapsedMs }`. Reporting failures are logged but do not
   fail the cycle.

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
