# fusion-supervisor

Local-first fusion service supervisor and monitor. Long-lived Rust daemon `fusion-sv` supervises all 44 fusion services registered in `architecture/port-registry.yaml`: start/stop/restart, health probing, crash auto-recovery (crash-window + exponential backoff), health history in SQLite, and alerting.

## Why

The fusion monorepo had no persistent supervisor. Existing tools are one-shot or display-only: `fusion service status --watch` (6 services, no restart), `fusion doctor` (5 services), `fusion-autotest` (~20, exits on completion), `fusion-studio/Scripts/start.sh` (14, ends after start). 44 services with no continuous watcher, no crash restart, no health history aggregation, no unified liveness path (`/health` vs `/healthz` vs UDS-ping chaos). fusion-supervisor fills this gap.

## Install

```bash
cd fusion-supervisor
cargo build --release   # binary: target/release/fusion-sv
```

## Usage

```bash
fusion-sv daemon          # start supervisor daemon (long-lived)
fusion-sv status          # list all 44 services + state (talks to daemon over UDS)
fusion-sv top             # live status table, 1s refresh
fusion-sv up              # start all services (layer order: core→platform→app→domain→cluster)
fusion-sv down            # stop all (reverse layer order)
fusion-sv restart <svc>   # restart one service (resets Failed state)
fusion-sv logs <svc>      # tail last 50 lines of per-service start.sh log
fusion-sv ping            # ping the daemon (alive check, prints pong)
```

## How it works

- **Source of truth:** reads `architecture/port-registry.yaml` for the 44-service manifest. Never rewrites `start.sh` scripts — orchestrates them (`bash <repo>/start.sh start|stop|restart`).
- **Dual-mode binary:** `daemon` = long-lived supervisor loop + UDS JSON-RPC server (`/tmp/fusion-sv.sock`, perms 0600); CLI subcommands = one-shot client over the same UDS.
- **Health probing:** per-service `probe_map` normalizes liveness paths (HTTP `/health`/`/healthz`, UDS `ping`→`pong`, TCP fallback). 2s timeout.
- **Crash recovery:** ports fusion-mlx `--watchdog` — 300s sliding window, 5-crash cap, exponential backoff 1→2→4→8→16→30s. Cap hit → `Failed` state, stops restart, alerts (manual `fusion-sv restart <svc>` to reset).
- **Anti-restart-storm:** a 404/405 (`BadPath`) = probe path wrong, NOT a crash signal → never restarts, only alerts. Only `ConnectionRefused`/`Timeout` trigger restart. The crash-window cap is the final backstop.
- **History:** SQLite (`~/.fusion-sv/history.db`, WAL mode), `health_check` + `event` tables, 7-day retention.
- **Alerts:** `ServiceDown` (deduped 5min/service), `ServiceBack`, `RestartFailed`, `RestartCapHit` → log + optional webhook.

## Config

Optional `~/.fusion-sv/config.toml` (all fields have defaults):

```toml
socket_path = "/tmp/fusion-sv.sock"
db_path = "~/.fusion-sv/history.db"
poll_healthy_sec = 15
poll_unhealthy_sec = 2
max_concurrent_start = 4
start_timeout_sec = 30
restart_window_sec = 300
restart_max = 5
backoff_max_sec = 30
alert_url = ""        # empty = log only
alert_dedup_sec = 300
registry_path = "architecture/port-registry.yaml"
```

Optional auth: set `FUSION_SV_TOKEN` env on daemon; CLI/clients pass `params.token`.

## Boundary with existing supervisors

- **fusion-gateway** stays the inference routing entry. Its `auto_start` only starts fusion-mlx; supervisor manages all 44 (including mlx). Double-start avoided: supervisor probes first, skips if already healthy. (Boundary tracked in fusion-gateway issue.)
- **fusion-studio/Scripts/start.sh** → legacy. Supervisor uses registry truth, does not inherit the script's stale values. (Studio migration tracked in fusion-studio issue.)
- **fusion-cli** `service`/`rag`/`doc` start/stop remain for interactive convenience; supervisor is authoritative. (Optional thin `fusion net` subcommand tracked in fusion-cli issue, v1.1.)

## Test

```bash
cargo test --all-targets          # ~40 unit + 9 integration
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Acceptance (v1)

- [x] `cargo build --release` → `fusion-sv`, 0 warnings (clippy -D warnings)
- [x] `cargo test` layer 1+2 all pass (49 tests)
- [ ] `fusion-sv daemon` then `fusion-sv status` lists all 44 services (real registry)
- [ ] `fusion-sv up` starts core (mlx+gateway), skips already-running (no double-start)
- [ ] Manually kill a non-core service → recovers within 30s, history recorded
- [ ] Kill same service 5x → `Failed` state, stops restart, `RestartCapHit` alert
- [ ] `fusion-sv down` reverse-order stop, confirms `Stopped`
- [ ] Probe path error (404) → no restart (anti-storm), alert only
- [ ] `fusion-sv top` readable live table, per-service log files
- [x] Process data cleaned after testing, only final binary + logs kept
- [x] README generated (this file)

## Non-goals (v1)

Prometheus aggregation (v2), cross-node cluster orchestration, rewriting start.sh, Web UI, launchd self-guard (v1.1), config hot-reload, multi-user.
