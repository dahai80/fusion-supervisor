# fusion-supervisor

Local-first fusion service supervisor and monitor. Long-lived Rust daemon `fusion-sv` supervises all 53 fusion services registered in `architecture/port-registry.yaml`: start/stop/restart, health probing, crash auto-recovery (crash-window + exponential backoff), health history in SQLite, and alerting.

## Why

The fusion monorepo had no persistent supervisor. Existing tools are one-shot or display-only: `fusion service status --watch` (6 services, no restart), `fusion doctor` (5 services), `fusion-autotest` (~20, exits on completion), `fusion-studio/Scripts/start.sh` (14, ends after start). 53 services with no continuous watcher, no crash restart, no health history aggregation, no unified liveness path (`/health` vs `/healthz` vs UDS-ping chaos). fusion-supervisor fills this gap.

## Install

```bash
cd fusion-supervisor
cargo build --release --workspace   # binaries: target/release/fusion-sv (daemon) + target/release/fusion-tray (desktop tray, issue #62)
```

## Usage

```bash
fusion-sv daemon          # start supervisor daemon (long-lived)
fusion-sv status          # list all services + containers + state (native + compose planes)
fusion-sv top             # live status table, 1s refresh
fusion-sv up              # start all (compose sidecar → native layer order → compose business)
fusion-sv down            # stop all (compose down → native reverse layer order)
fusion-sv restart <svc>   # restart one native service (resets Failed state)
fusion-sv logs <svc>      # tail last 50 lines (compose container logs OR native start.sh log)
fusion-sv ping            # ping the daemon (alive check, prints pong)
fusion-sv drain [svc]     # drain traffic from one service or all (zero-downtime prep) — issue #9
fusion-sv rollout <svc>   # zero-downtime rollout: drain→stop→start→prewarm — issue #9
fusion-sv backup          # one-shot backup: Postgres (pg_dumpall→gzip) + Qdrant snapshot → NAS — issue #10
fusion-sv shutdown        # gracefully shut down the daemon over the UDS socket — issue #20
```

**FUSION_ROOT (required in practice).** Default start paths resolve as `bash $FUSION_ROOT/<repo>/start.sh`. `FUSION_ROOT` env wins; when unset the daemon falls back to its cwd — so `cd fusion-supervisor && fusion-sv daemon` resolves every default start path relative to the wrong directory and `up` fails closed with "脚本不存在" errors. Start the daemon from the monorepo root instead:

```bash
cd ~/fusion && FUSION_ROOT=~/fusion fusion-sv daemon
```

The daemon logs a one-line warning at boot when `FUSION_ROOT` is unset and the cwd does not look like the monorepo root (no `architecture/port-registry.yaml`).

**Registry exemptions (issues #31/#33/#34).** Registry entries may set `supervise: false` — the supervisor skips them entirely (no runner, no probe, no restart, not in status): they are dev-only servers, embedded sub-listeners, k8s deploy artifacts, or not-yet-built ports that have nothing to supervise locally. Principle (issue #33): every registry entry is either supervisable or explicitly exempted, so a boot error always means a genuinely broken service. Exempted: `fusion-web-shell` (Vite dev; production serves static dist from the gateway), `fusion-gateway-deploy` (k8s/helm artifact), `fusion-comfyui-frontend` (Phase 4, not built), plus the #33 batch — Vite dev consoles whose production path is gateway/daemon static hosting (`fusion-supervisor-web`, `fusion-model-hub-web`, `fusion-mlx-web`, `fusion-doc-web-dev`), embedded sub-listeners with no separate process (`fusion-identity-grpc` = fusion-identity's in-process gRPC; `fusion-simulation-gui`/`-metrics` = fusion-sim's in-process dashboard/metrics), and not-yet-built (`fusion-autotest-ci-gateway`) — and the #34 batch: more Vite dev consoles (`fusion-cowork-web-dev`, `fusion-multi-node-web`, `fusion-security-web`, `fusion-bench-web`, `fusion-bench-benchsite`), embedded/optional in-process listeners (`fusion-cowork-sync` = cowork sync engine, opt-in via `FUSION_COWORK_CROSS_SYNC=1`; the `fusion-multi-node-agent/-mcp/-sync/-mdns/-discovery` cluster plane, worker-plane or in-master with no default separate listener).

**Supervised without start.sh (issues #33/#34).** Services whose real start path differs from `bash <repo>/start.sh` are wired via `start_map` overrides: `fusion-store` (daemonizes `fusion-store/target/release/fs-serve` directly — `deploy/start.sh` has an upstream binary-name bug, fusion-store#5), `fusion-smallbusiness` (daemonizes the `fusion-smallbusiness` uvicorn CLI), `fusion-simulation` (daemonizes `fusion-sim service start --host 127.0.0.1 --gui`; gRPC probed over TCP, no HTTP face), `fusion-plugins-ecosystem` (:8765, daemonized as `fusion-plugin-server --transport sse --port 8765`, TCP probe; the MCP server has no `/health`), plus the #34 batch of broken-upstream-start.sh services — `fusion-cowork-ws` (`fusion-cowork remote serve`; WebSocket, TCP probe), `fusion-bench` (`fusion-bench serve`; start.sh references a deleted module, fusion-bench#27), `fusion-k12-teacher` (`fusion-k12 serve`; start.sh takes no `start` subcommand, fusion-k12-teacher#31), `fusion-code-modenization` (`python -m fusion_code_modelization.server`; start.sh imports a module with no `__main__`, fusion-code-modelization#21). All verified live.

**Probe path alignment (issue #34).** Fleet-wide probe-path drift fixed in `probe_map`: `fusion-model-hub`/`fusion-bench` probe the auth-exempt `/api/v1/system/health`, `fusion-health` `/api/v1/health`, `fusion-finance` `/api/v1/ready`, `fusion-k12-teacher` `/api/health`, `fusion-memory` HTTP `/healthz` (was UDS), `fusion-speech` HTTP `/health` (socket moved upstream), `fusion-projects` UDS (registry port 11440 is dead), `fusion-code`/`fusion-cowork-ws` TCP (endpoints are auth-gated / WebSocket — HTTP probing would 401 forever). Known remaining: `fusion-code-modenization` /health 503s until upstream fixes its gateway check (sends no Authorization header, fusion-code-modelization#22) — supervisor correctly reports Unhealthy without restart churn.

**Daemon env (issue #34).** Children inherit the daemon's environment. Start the daemon with the shared venv active and the monorepo root `.env` sourced, or Python services crash-loop with `No module named ...` and gateway-authed health checks 401:

```bash
cd ~/fusion && source .venv/bin/activate && set -a && source .env && set +a && FUSION_ROOT=~/fusion fusion-sv daemon
```

**Env stripping for settings-owned services (issues #39/#40/#45).** Inheritance cuts both ways: `fusion-mlx`'s `start.sh` gives `FUSION_MLX_API_KEY` env precedence over `~/.fusion-mlx/settings.json` `auth.api_key`, so a daemon carrying a stale key from the monorepo `.env` would pin that snapshot into every supervisor-triggered relaunch — all settings.json clients (gateway backend, scrapers) 401 while the service still probes Healthy. The supervisor now strips `FUSION_MLX_API_KEY` from the `fusion-mlx` child env (`env_map::strip_env_for`), so each launch resolves the key fresh from settings.json — single source of truth. Issue #45 extends the same strip to `fusion-health` (its `config.py` also prefers the env var over settings.json — a stale daemon snapshot made backend probes 401 and `/api/v1/health` report 503 degraded). Applies to all spawn paths: `up_all`, `up_one`, and crash-recovery restarts. The compose `check_env` guard still requires the daemon itself to hold the var (it gates compose containers, which genuinely need it).

## Business-plane takeover (issue #45)

Four business services are registered in `architecture/port-registry.yaml` WITHOUT the `fusion-` prefix, with repos under `business/<name>/` in the monorepo:

| Service | Port | Start command (start_map Daemon spec) | Probe |
|---|---|---|---|
| ai-family | 11483 | `bash business/ai-family/start.sh start` (default path; proper start/stop/status case statement) | TCP (every router is `verify_service_key`-gated — HTTP would 401 forever) |
| ai-fit | 11510 | daemonized `uvicorn server.main:app --port 11510`, cwd `business/ai-fit` (issue #55: registry key and dir both renamed `fit-ai`→`ai-fit`; dropped the broken `--app-dir server` — uvicorn imports from repo root) | HTTP `/api/v1/system/health` (auth-exempt) |
| ai-speech-exam | 11560 | daemonized `uvicorn app.main:app --app-dir backend --port 11560`, cwd `business/ai-speech-exam`, env `FUSION_AI_SPEECH_EXAM_CONFIG=config/config.yaml` | HTTP `/health` (unauthenticated) |
| ai-apparelmockup | 11520 | daemonized `uvicorn server.app:app --port 11520`, cwd `business/ai-apparelmockup` (issue #55: upstream switched from static http.server to FastAPI) | TCP (no public health route observed — HTTP could 404 = BadPath forever) |

Issue #55 extends the business plane to the 2026-10-27/28 port-governance batch (all `StartSpec::Daemon`, pinned to registry ports where upstream defaults still disagree):

| Service | Port | Start command | Probe |
|---|---|---|---|
| ai-ads-videobuilder | 11531 | `python -m ads_vb.cli serve --port 11531`, cwd `business/ai-ads-videobuilder` | TCP (no public health route) |
| ai-doc-automation | 11550 | `python -m backend.cli serve --port 11550`, cwd `business/ai-doc-automation` | HTTP `/health` |
| ai-information-search | 11595 | `python -m backend.app`, env `FUSION_AI_INFORMATION_SEARCH_PORT=11595`, cwd `business/ai-information-search` | HTTP `/health` (auth-exempt) |
| ai-OperaBase | 11504 | `python -m fusion_ai_operabase.main`, env `FUSION_AI_OPERABASE_PORT=11504`, cwd `business/ai-OperaBase` (dir casing ≠ registry key) | TCP (no public health route) |
| ai-charactervoice | 11505 | `uvicorn svc_api.app:app --port 11505`, cwd `business/ai-charactervoice` (config default 11480 — port pinned until upstream migration lands) | HTTP `/health` |
| ai-charactervoice-session | 11506 | `uvicorn svc_session.app:app --port 11506`, same repo as above (default 11481) | HTTP `/health` |
| customer-insight | 11594 | `python -m http.server 11594 --directory web/dist`, cwd `business/ai-customer-insight` | TCP (static server) |
| customer-insight-api | 11501 | `python -m http.server 11501 --directory web/dist`, same repo (serves static until the real API lands upstream) | TCP (static server) |
| ai-apparelmockup-web | 11593 | `python -m http.server 11593 --directory web/dist`, cwd `business/ai-apparelmockup` | TCP (static server) |

Three registry entries are explicitly exempted (`supervise: false`) until their upstream blockers clear: `ai-ads-videobuilder-web-dev` (dev-only vite server), `envipermit` (containerized design; its deploy compose maps API 11510 which now collides with ai-fit), `ai-surlsch00l-bookingagent` (`surl-agent serve` hardcodes 11570 ≠ registry 11507; migration TASK pending upstream, no git remote to file an issue).

Three hazards drove the design:

1. **Repo-dir mapping**: `manifest::name_to_dir` maps the registry keys to `business/<name>` (default `~/fusion/<name>` does not exist — every start path failed closed).
2. **Arg-less start.sh**: `ai-speech-exam` (and formerly `fit-ai`/`ai-apparelmockup`) start.sh scripts `exec` a foreground process and IGNORE subcommand args — `bash start.sh stop` would START the service. They are taken over via `StartSpec::Daemon` (issue #34 pattern), which now supports `cwd` (resolved against `FUSION_ROOT`) and app-internal `env`. Daemon pid discipline: `mkdir`/`cd ... || exit 1`/`export` joined with `;` (never `&&` — `A && B &` backgrounds the whole list and the pidfile would hold the subshell pid, orphaning the service on stop).
3. **FUSION_CLIENT_KEY injection**: the FastAPI business apps (issue #55: `ai-fit`, `ai-speech-exam`, `ai-apparelmockup`, `ai-ads-videobuilder`, `ai-doc-automation`, `ai-information-search`, `ai-OperaBase`, `ai-charactervoice` ×2) fail-closed at boot without `FUSION_CLIENT_KEY` (gateway client key, 约定 1 — they call the gateway LLM channel). `env_map::shell_env_for` injects it on every spawn path (up_all/up_one/crash-restart): process env → `.env` → read-only fallback to `fusion-gateway/config.yaml` `auth.api_keys` (prefers the `name=admin` entry). All three sources missing → the supervisor refuses to start the service (fail-closed) rather than spawn a guaranteed crash-loop. `ai-family` (own `verify_service_key` auth) and the pure static hosts (`customer-insight` ×2, `ai-apparelmockup-web` — no gateway calls) are deliberately excluded.

Restart semantics: `ai-family`'s start.sh has no `restart` subcommand, and Daemon-spec services have no restart semantics at all — `restart` falls back to stop+start (`probe_map::restart_cmd_for`).

Deploy note: `ai-speech-exam` reads `config/config.yaml` (git-ignored; only `config.example.yaml` is committed, containing no secrets). Copying the example is an accepted deploy step.

**Web console vendored SDK re-sync (issue #43).** `web/vendor/@fusion/web-sdk` is re-vendored from `fusion-web-shell` tag **v0.1.2**: picks up the host-bridge `isHttpErrorResult()` type-predicate fix (bare-string bridge results no longer throw `TypeError: Cannot use 'in' operator`, upstream #75) and the cached-sync `theme.get()` seeded by a background host pull (upstream #77). The local `?embedded=1`-without-parent-frame patch (top-level deep links) is re-applied on top — upstream never carried it.

When `compose.enabled = true`, `up`/`down`/`status`/`logs` span BOTH the 53 native services AND the container planes (Traefik+Redis+Postgres+Qdrant sidecar + model-hub/cowork business) in one command — true one-command bring-up with unified monitoring. Disabled by default (native-only, preserves single-node dev behavior).

## How it works

- **Source of truth:** reads `architecture/port-registry.yaml` for the 53-service manifest. Never rewrites `start.sh` scripts — orchestrates them (`bash <repo>/start.sh start|stop|restart`).
- **Registry-key → repo-dir mapping (issue #57):** `name_to_dir` maps registry keys to local repo directories. Most keys equal their directory; known exceptions live in an explicit table (`fusion-code-modelization` → `fusion-code-modenization` spelling, business-plane keys → `business/<name>`, and `fusion-multi-node` → `fusion-multi-nodes` — the registry key is singular but the repo directory on disk is plural; before #57 the default mapping pointed at a nonexistent directory and the Cluster master failed closed on every boot). Sub-service keys (`fusion-cowork-ws`, `fusion-multi-node-agent`, …) resolve to their parent repo via a `(prefix, dir)` table, so plural-directory parents resolve correctly too.
- **Dual-mode binary:** `daemon` = long-lived supervisor loop + UDS JSON-RPC server (`/tmp/fusion-sv.sock`, perms 0600); CLI subcommands = one-shot client over the same UDS.
- **Health probing:** per-service `probe_map` normalizes liveness paths (HTTP `/health`/`/healthz`, UDS `ping`→`pong`, TCP fallback). 2s timeout.
- **Crash recovery:** ports fusion-mlx `--watchdog` — 300s sliding window, 5-crash cap, exponential backoff 1→2→4→8→16→30s. Backoff is scheduled, not slept in-lock (issue #31): the tick records a `restart_after` deadline and returns, so `/metrics` and status RPCs stay responsive while failing services churn. Cap hit → `Failed` state, stops restart, alerts (manual `fusion-sv restart <svc>` to reset). `up [svc]` / `down [svc]` are true single-service operations end-to-end (issue #31 fixed the RPC side — `up_one`/`down_one`; issue #41 fixed the CLI side, which parsed the `[SERVICE]` argument but silently dropped it, so `down <svc>` stopped the whole fleet — the arg now routes through the tested `cli::up_down_target` helper).
- **Anti-restart-storm:** a 404/405 (`BadPath`) = probe path wrong, NOT a crash signal → never restarts, only alerts. Only `ConnectionRefused`/`Timeout` trigger restart. The crash-window cap is the final backstop.
- **Config-fatal backoff + fast-fail circuit breaker (issue #47):** when a start attempt exits with code 78 (`EX_CONFIG` — the fusion-gateway contract for "config corrupt, restart is useless"), the supervisor stops hammering: backoff escalates 10s→60s→300s (instead of the normal 1→2→4 fast sequence), a `ServiceConfigFatal` alert fires with the child's stderr tail attached, and 5 consecutive config-fatal exits mark the service `Failed` (manual `fusion-sv restart <svc>` to reset once the config is fixed). Independently, a fast-exit breaker watches every service: 3 consecutive crashes within 10s of a successful start trips a `FastFailLoop` alert and switches that service to the escalated backoff too. Both breakers reset on recovery to Healthy. Failure-reason surfacing: start failures capture the last 20 stderr lines of the child, so `RestartFailed`/`ServiceConfigFatal` alert details — and the history.db event rows behind `/api/v1/events` — carry the actual root cause, not just an exit status.
- **Detached service process groups (issue #60):** every service-start child (`exec_bash_script` + `StartSpec::Daemon` exec) is spawned with `process_group(0)` — it becomes its own process-group leader instead of inheriting the daemon's group. Why: reloading the launchd-managed daemon (`launchctl bootout`) signals the job's whole process group, which used to SIGTERM the entire supervised fleet in one shot (`nohup` only blocks SIGHUP, not a group signal). With detached groups a daemon reload touches only the daemon; the next boot re-adopts the still-running services via probe-first-skip-healthy, so recovery is a no-op instead of a mass restart. Stop paths are unaffected (they address processes by pidfile, not by group). Regression test asserts the exec'd child lands in a different pgid.
- **Non-blocking stderr capture (issue #51):** the issue #47 stderr tail is captured WITHOUT pipes — child stderr is redirected straight into the per-service log file and the tail is read back after `wait()`. The previous pipe + reader-thread implementation blocked forever on services whose `start.sh` daemonizes descendants (uvicorn workers, backgrounded servers inherit the pipe write end and never close it): the `join()` waited for pipe EOF, the start semaphore permit leaked, `up_all` stalled while holding the core mutex, and every RPC + `/api/v1/status` hung (only lock-free `/health` answered). Trigger: fresh host boot with `auto_up_on_start=true`. A regression test asserts `exec_bash_script` returns promptly even with a lingering grandchild holding stderr.
- **Per-service `.env` sourcing for daemonized overrides (issue #49):** `StartSpec::Daemon` gains an `env_file` field — before nohup, the command does `set -a; [ -f <file> ] || exit 1; source <file>; set +a` (path resolved against `FUSION_ROOT`; missing file = fail-closed refuse-to-start). `fusion-k12-teacher` uses it to source its own `.env` — the project's key material (`FUSION_MLX_API_KEY` etc.) lives there, and a bare `fusion-k12 serve` previously inherited the daemon's env snapshot with a stale rotated key: gateway 401 → `/api/health` 503 `backend: down` → probe restart loop every ~5 min. Also from #49: a non-restart-eligible probe failure (`BadPath`/`BadStatus`) now emits its `RestartFailed` alert only on the transition into `Unhealthy` — sustained-unhealthy silence instead of one alert per 2s tick; recovery re-arms it.
- **Secret placeholders in `.env.example` (issue #50):** the gateway key line is acquisition guidance (`obtain-via-gateway-admin-api`), not a fake usable key — the old `fg-admin-key` literal copied straight into real `.env` files produced 401s with a "correct-looking" config. Aligns with `architecture/config-conventions.md` §4; `FUSION_MLX_API_KEY` keeps its name (supervisor + compose consume it; it is the documented deprecated alias of `FUSION_CLIENT_KEY`, conventions §6).
- **History:** SQLite (`~/.fusion-sv/history.db`, WAL mode), `health_check` + `event` tables, 7-day retention. The retention sweep is wired (issue #37): the daemon tick checks `prune_schedule_sec` (default hourly) and prunes rows older than `history_retention_days` (default 7) from BOTH tables; pruned-row count is logged per sweep and accumulated in `fusion_sv_events_pruned_total`. Failed sweeps also advance the schedule timestamp (no per-tick retry storm — same semantics as the backup schedule).
- **Alert persistence (issue #37):** every alert that survives dedup is persisted to the `event` table (kind, detail, ts, plane, severity) at the `Alerter::alert` chokepoint — native and compose alerts alike (compose services stored unprefixed with `plane="compose"`). The event table carries a `severity` column (`error` = container crash-loop, `info` = recovery/drain/backup, `warn` = the rest); old DBs are migrated idempotently (`ALTER TABLE ... DEFAULT 'warn'`). So the web console's Alerts page and `/api/v1/events` answer "what alerted last week". `fusion_sv_alerts_total` counts emitted alerts.
- **Alerts:** `ServiceDown` (deduped 5min/service), `ServiceBack`, `RestartFailed`, `RestartCapHit` → log + optional webhook.
- **Compose plane (optional):** `docker compose` wrapped behind `ComposeBackend` trait. `up` = sidecar (`--wait`) → native (layer-ordered) → business (`--wait`); `down` = compose `down --remove-orphans` → native reverse. `status` merges native + container rows (tagged by `plane`); `tick` alerts on exited/unhealthy containers (monitored, not crash-restarted — Docker restart policy owns that). Fail-closed on `FUSION_PG_PASSWORD`/`FUSION_MLX_API_KEY`. See `src/compose.rs`.
- **NAS backup (issue #10):** `fusion-sv backup` (CLI or RPC) dumps Postgres (`pg_dumpall` → gzip) and snapshots Qdrant (`storage` tar) to `FUSION_BACKUP_TARGET` (`{target}/fusion-pg-{date}.sql.gz`, `{target}/fusion-qdrant-{date}/`). Scheduled by the daemon tick loop via `backup_schedule_sec` (default daily); retention `backup_retention` (default 7) prunes older sets. **Fail-closed:** `FUSION_BACKUP_TARGET` unset → refuses + `BackupFailed` alert (production safety task never silently skipped). Outcome recorded in history.db as `kind=Backup`. See `src/backup.rs`.
- **Remote shutdown (issue #20):** `fusion-sv shutdown` (CLI or RPC) sends a one-shot signal over the UDS socket. The daemon's main loop selects on `ctrl-c` OR this signal, so either triggers the same graceful path (abort tick loop, return). Idempotent: a second `shutdown` call returns `-32000 shutdown channel unavailable` (the signal can only be sent once). `top` is a client-side command (`fusion-sv top` loops `fusion-sv status` with a 1s clear-screen refresh) — its server RPC arm returns `-32601` with a message directing you to run `fusion-sv top` directly; no server RPC is needed.
- **Prometheus /metrics (issue #22):** the daemon serves a `/metrics` HTTP endpoint (Prometheus text exposition format v0.0.4) on `metrics_addr` (default `127.0.0.1:9100`, localhost-only — no auth, the binding is the security boundary). Set `metrics_addr = ""` (or omit) to disable. Hand-rolled HTTP server (`tokio::net::TcpListener` + `tokio::io`) — no hyper/axum/prometheus crate added. Lock discipline: handler locks core → `metrics::render` (pure function) → releases immediately, same short-lock pattern as RPC; never held across `.await`. Metrics: `fusion_sv_up 1` (daemon alive), `fusion_sv_services_total{plane}` (count by plane), `fusion_sv_service_healthy{service,port,plane}` (1/0), `fusion_sv_service_state{service,port,plane,state}` (1 for current state), `fusion_sv_crash_count{service}` (sliding-window crash count, native only), plus issue #37 counters `fusion_sv_alerts_total` (alerts emitted post-dedup) and `fusion_sv_events_pruned_total` (history rows pruned by retention sweeps). See `src/metrics.rs`.
- **Read-only HTTP status (issue #26):** the same HTTP listener also serves `GET /api/v1/status` — a JSON array mirroring the UDS `status` RPC (`[{name, state, port, plane}]`, wrapped as `{"ok": true, "services": [...]}`). Browser-reachable data source for web ops dashboards; loopback-bound with the metrics listener (the binding is the security boundary). Read-only by design: start/stop/restart stay on the token-authed UDS RPC; unknown paths 404. See `metrics::render_status_json` in `src/metrics.rs`.
- **Web console (issue #25):** the HTTP face grows into a full console surface on the same `:9100` listener (`src/http_server.rs`): `GET /api/v1/history?service=&limit=` (per-service health history from the SQLite store), `GET /api/v1/events?limit=` (global event stream — the alerts data source), `GET /api/v1/logs?service=` (log tail), and `POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}` — mutations reuse the UDS `dispatch` unchanged, so token semantics are identical (`Authorization: Bearer` / `x-token` header maps to the RPC `token` param). With `web_dir` configured the daemon also serves the built SPA from `web/dist` (`/` → `index.html`, path-traversal rejected, text assets only). The console itself lives in `web/` (React 18 + Vite + `@fusion/web-ui`, dev port 11473): Services page (status table + up/restart/drain/rollout with danger-confirm dialogs, spec S5), Health page (per-service latency chart), Alerts page (event stream). See `web/README.md`.

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
metrics_addr = "127.0.0.1:9100"   # Prometheus /metrics (issue #22) + /api/v1/* face + web console (issue #25); "" = disable all
web_dir = "~/fusion/fusion-supervisor/web/dist"  # optional: serve the built web console from the same listener; "" = off
auto_up_on_start = true            # issue #29/#62: daemon boot runs up_boot_set once — T0 (fixed core) + pin=auto services (probe-skips already-healthy); host-reboot self-healing when the daemon itself is launched at boot. Default false. BEHAVIOR CHANGE in #62: was up_all (everything).

# Compose plane orchestration (disabled by default — native-only)
[compose]
enabled = false                    # set true to orchestrate container planes too
sidecar_file = "docker-compose.sidecar.yml"
business_file = "docker-compose.business.yml"
env_file = ".env"                  # fail-closed: needs FUSION_PG_PASSWORD + FUSION_MLX_API_KEY

# NAS backup (issue #10) — fail-closed if FUSION_BACKUP_TARGET unset
backup_schedule_sec = 86400        # daemon tick checks; 0 = first run always due. Default daily.
backup_retention = 7               # keep newest N pg+qdrant backup sets, prune older (issue #10)

# history.db retention (issue #37) — sweep prunes health_check/event rows older than N days
history_retention_days = 7
prune_schedule_sec = 3600          # how often the sweep runs
```

Every field is optional in the file — write only the keys you want to override (all have serde defaults since v0.1.1; partial configs used to fail with `missing field socket_path`).

Optional auth: set `FUSION_SV_TOKEN` env on daemon; CLI/clients pass `params.token`.

## Shell-ecosystem takeover (issue #29)

Four shell module services are supervised with custom start commands + registration env (the 2026-10-24 incident: a host reboot silently killed them and the shell sidebar lost every product entry):

| Service | Port | Start command | Required env (injected, fail-closed) |
|---|---|---|---|
| fusion-cowork-web | 11461 | `fusion-cowork web serve --port 11461` (daemonized w/ pidfile) | `FUSION_SHELL_GATEWAY_URL`, `FUSION_ADMIN_KEY` |
| fusion-trainer-bridge | 8790 | `fusion-trainer/scripts/trainer-bridge.sh start` | `FUSION_TRAINER_BRIDGE_KEY` |
| fusion-plugins-ecosystem-console | 8795 | `fusion-plugin-console --port 8795` (daemonized w/ pidfile) | `FUSION_GATEWAY_BASE_URL`, `FUSION_ADMIN_KEY` |
| fusion-agent-studio | 11442 | `start.sh start` (default path) | — |

Env resolution: process env → `.env` (compose `env_file`) → fallback. `FUSION_ADMIN_KEY` (issue #54: renamed from `FUSION_GATEWAY_ADMIN_KEY` per env conventions Phase 2.2) falls back to read-only `web_shell.admin_key` from `fusion-gateway/config.yaml` (same pattern as trainer-bridge.sh); `FUSION_GATEWAY_BASE_URL` (issue #54: renamed from `FUSION_SHELL_GATEWAY_URL`) falls back to `http://127.0.0.1:11432`. `fusion-cowork`'s shell registry still reads the old URL name during the upstream transition window, so `fusion-cowork-web` keeps `FUSION_SHELL_GATEWAY_URL` + `FUSION_ADMIN_KEY`; the plugins console (v0.4.7) reads the new names only. `FUSION_TRAINER_BRIDGE_KEY` has no fallback — put it in `.env` (git-ignored). Gateway is Core layer, modules are App layer, so `up_all` brings up the gateway before the modules register against it.

## Boot tiering + desktop tray (issue #62)

Implements `architecture/fusion-supervisor-app-tiering-prd-1029.md`: services split into boot tiers so a host reboot brings up only what matters, plus an egui floating panel for fleet control.

**Tiers** (annotated as `boot: {tier, pin}` per service in `architecture/port-registry.yaml` — 88 entries):

- **T0 core** (10): mlx, gateway, memory, speech, identity, model-hub, rag, trainer, cowork, supervisor-adjacent core — always started at boot, cannot be skipped without a `manual` pin.
- **T1 platform** (~20): everyday work services — started at boot only when pinned `auto` (per-user override, stored in `~/.fusion-sv/history.db` table `boot_pin`), otherwise on-demand.
- **T2 app/domain** (rest): never started at boot; started on demand (tray / web console / CLI).

The boot set = T0 ∪ {pinned=auto}. `auto_up_on_start = true` in `~/.fusion-sv/config.toml` now runs **`up_boot_set`** (T0 + pins) instead of `up_all` — **behavior change**: previously every one of the 53+ services was dragged up on daemon boot; now host-reboot recovery only restores the core + your pinned platform services, cutting boot memory footprint and start churn. `fusion-sv up` with no argument still means all services.

HTTP face additions (same token rules as other mutations):

- `GET /api/v1/boot` — one row per service: `{name, tier, pinned, state, port}` (single endpoint; the tray polls only this — it carries tier+pin+state, superset of `/api/v1/status`).
- `POST /api/v1/boot/pin` `{"service": "...", "mode": "auto"|"manual"|"clear"}` — per-user boot override; survives daemon restart (SQLite).
- `POST /api/v1/boot/group` `{"group": "T0"|"T1"|"T2", "action": "up"|"down"}` — tier-level bulk ops.

### Desktop tray (`fusion-tray`)

```bash
cargo build --release --workspace   # binary: target/release/fusion-tray
./target/release/fusion-tray        # no config needed; env overrides below
```

A 14px dot docked at the right screen edge (vertical center): green = all T0 healthy, yellow = T0 degraded, red = T0 down, gray = daemon unreachable. Click → fan menu opening leftward from the dot (vertical semicircle: top = CORE, middle = PLATFORM, bottom = APP; 3 tier sectors with per-tier aggregate color and per-tier service dots) → click a sector → service list (running-first sort, realtime search, per-service start/stop/pin, right-click restart/drain/rollout; CORE stop needs confirm). **Every tier has both one-click up and one-click stop** (`POST /api/v1/boot/group`; group stop is destructive and confirms first). ESC walks back list → fan → dot. Destructive ops (restart/drain/rollout/group-down/all-stop) confirm first.

When the daemon is unreachable the dot turns gray and the panel shows a "daemon 未运行" notice with one-click self-heal (`fusion-sv up`); no white screen. Unreachable is decided by a two-step probe: `GET /api/v1/boot` first, then `GET /health` (lock-free handler) as a busy-vs-dead discriminator — a timed-out `/boot` with a live `/health` keeps the current view and logs "daemon 忙" instead of flashing the unreachable notice (batch group ops hold the core mutex for seconds). Env: `FUSION_SV_BASE` (default `http://127.0.0.1:9100`), `FUSION_SV_TOKEN`, `FUSION_TRAY_THEME` (`dark`/`light`, default follows system appearance). Poll cadence: 5s panel open / 30s dot / 60s unreachable.

**Resident-process robustness + memory** (PRD audit hardening):

- **CJK font subset**: the tray embeds `tray/assets/cjk-subset.ttf` (~132KB, `include_bytes!` + `FontData::from_static`, zero-copy) instead of reading the 22.4MB system Hiragino Sans GB at runtime — the full font used to stay resident as 3 malloc copies (~70MB) plus glyph caches, pushing RSS to ~180MB. After subsetting: RSS ~100MB (GPU/GL baseline included), `MALLOC_LARGE` 69MB → 2MB. After adding new Chinese text run `bash tray/gen_font_subset.sh && cargo test -p fusion-tray`; `font_tests::test_subset_font_covers_source_charset` (ttf-parser) fails the build if the subset drifts from the source charset.
- **Panic isolation**: poller and worker threads wrap each iteration in `catch_unwind` (a panic logs ERROR and continues instead of killing the tray); the daemon tick loop spawns each round in an inner task so a `tick_all` panic cannot silently stop supervision while RPC/HTTP stay alive. All tray mutexes use poison-recovery locks (`.lock().unwrap_or_else(|p| p.into_inner())`) so a panicked thread cannot cascade.
- **Transparent dot window**: the L1 dot window overrides `eframe::App::clear_color` to fully transparent (eframe's default `rgba(12,12,12,180)` painted a gray square behind the half-circle dot); fan/list/unreachable states keep their opaque panel backgrounds.

### LaunchAgent (log in → tray resident)

```bash
cp scripts/com.fusion.tray.plist.template ~/Library/LaunchAgents/com.fusion.tray.plist
# edit: replace __FUSION_SUPERVISOR_DIR__ with the real path
launchctl load ~/Library/LaunchAgents/com.fusion.tray.plist
```

## Boundary with existing supervisors

- **fusion-gateway** stays the inference routing entry. Its `auto_start` only starts fusion-mlx; supervisor manages all 53 (including mlx). Double-start avoided: supervisor probes first, skips if already healthy. (Boundary tracked in fusion-gateway issue.)
- **fusion-studio/Scripts/start.sh** → legacy. Supervisor uses registry truth, does not inherit the script's stale values. (Studio migration tracked in fusion-studio issue.)
- **fusion-cli** `service`/`rag`/`doc` start/stop remain for interactive convenience; supervisor is authoritative. (Optional thin `fusion net` subcommand tracked in fusion-cli issue, v1.1.)

## Test

```bash
cargo test --all-targets          # 210 test fns (unit + 13 integration files)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Acceptance (v1)

- [x] `cargo build --release` → `fusion-sv`, 0 warnings (clippy -D warnings)
- [x] `cargo test` layer 1+2 all pass (49 tests)
- [ ] `fusion-sv daemon` then `fusion-sv status` lists all 53 services (real registry)
- [ ] `fusion-sv up` starts core (mlx+gateway), skips already-running (no double-start)
- [ ] Manually kill a non-core service → recovers within 30s, history recorded
- [ ] Kill same service 5x → `Failed` state, stops restart, `RestartCapHit` alert
- [ ] `fusion-sv down` reverse-order stop, confirms `Stopped`
- [ ] Probe path error (404) → no restart (anti-storm), alert only
- [ ] `fusion-sv top` readable live table, per-service log files
- [x] Process data cleaned after testing, only final binary + logs kept
- [x] README generated (this file)

## Non-goals (v1)

Cross-node Prometheus aggregation (v2), cross-node cluster orchestration, rewriting start.sh, launchd self-guard (v1.1), config hot-reload, multi-user. (Web console shipped anyway — see `web/`.)

## Container Multi-Node Deploy

Containerized multi-node commercial-deployment topology — extends
`architecture/fusion-deploy-container-multinode-0901.md`. Layered Docker
Compose: base cluster plane (`fusion-multi-nodes/docker-compose.yml`, network
pin #58 landed) + business-plane overlay (`docker-compose.business.yml` here).
Merge: `docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d`.

**What's containerized:** cluster plane (Master + Agent) + HTTP business plane
(model-hub, cowork now; gateway/rag/artifacts/code/doc gated on upstream
Dockerfile PRs). **What stays bare-metal:** MLX (`localhost:11434`, Metal /
unified memory), UDS guardian plane (memory/speech — UDS doesn't cross the
container boundary).

### Prerequisites

1. MLX running bare-metal: `~/claude-home/fusion-mlx/start.sh start`
2. Cluster token + MLX API key exported inline (the base compose is fail-closed
   `${FUSION_CLUSTER_TOKEN:?}` / `${FUSION_MLX_API_KEY:?}` — no `.env` is
   created in `fusion-multi-nodes`):
   ```bash
   export FUSION_CLUSTER_TOKEN=$(openssl rand -hex 16)
   export FUSION_MLX_API_KEY=fg-admin-key   # matches fusion-mlx api_key
   ```
3. Smoke model cached: `mlx-community-Llama-3.2-1B-Instruct-4bit`
   (download via `HF_MIRROR=https://hf-mirror.com` if missing), available in MLX.
4. Docker Desktop running (v29.4+).
5. Stop any bare-metal `fusion-multi-node` master first (it holds port 11452):
   `cd fusion-multi-nodes `cd fusion-multi-node && bash start.sh stop``cd fusion-multi-node && bash start.sh stop` bash start.sh stop`.

### Build the smoke images

The overlay Dockerfiles COPY monorepo-root-relative paths (requirements.lock,
`fusion-core/`, `fusion-cowork/`), so the build context must be the monorepo
root — but that is 200G+ with no root `.dockerignore`, making a root-context
build infeasible. The harness stages a lean build context instead:

```bash
bash scripts/prepare-build-ctx.sh   # idempotent: populates .build-ctx/ (git-ignored)
```

Then the harness builds `fusion-cowork:smoke` (real upstream Dockerfile) and
`fusion-model-hub:smoke` (scoped wrapper `scripts/Dockerfile.model-hub-smoke`
that installs model-hub's own pyproject deps instead of the full monorepo
lock — see upstream issue dahai80/fusion-models-hub#52).

### Run smoke validation

The multi-node smoke harness (`scripts/smoke-container-multinode.sh`) was
**removed** in PR #24 — it depended on `fusion-multi-nodes/docker-compose.yml`,
which is no longer tested on the local single-machine deployment. The
sidecar-plane smoke (`bash scripts/smoke-sidecar.sh` + unit-only
`bash scripts/smoke-sidecar.test.sh`) remains the validated path.

### Bring up topology without smoke

```bash
cd fusion-multi-node
docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d
```

Teardown: `docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml down --remove-orphans`

### Cleanup (process data)

Smoke harness auto-teardowns via `trap` — no persistent test data is kept,
only the `/tmp/fusion-smoke.*/smoke.log` run log remains (per CLAUDE.md:
keep only final output + logs). The `fusion-cluster` Docker network may
persist (harmless, reused across runs). For manual teardown + cert cleanup
(mTLS path): `docker compose ... down` + `rm -rf fusion-multi-nodes/tls/`.

## Production Sidecar (Traefik + Redis + Postgres + Qdrant)

Implements `architecture/fusion-deploy-0902.md` §1.1 — the OrbStack sidecar
middleware/data layer for the 10-person Phase 2 production deployment. The
native core (MLX + fusion-gateway, Metal/VRAM) stays on the host; sidecar runs
the reverse proxy, queue/lock/rate-limit store, relational metadata, and vector
DB in containers.

| Service | Image | Host port | Role |
| --- | --- | --- | --- |
| traefik | traefik:v3.1 | 80/443/8080 | TLS, domain routing, CORS, routes to native gateway + containerized apps |
| redis | redis:7-alpine | 6379 | quota/lock/rate-limit tokens (gateway `budget.go`, identity concurrency) |
| postgres | postgres:16-alpine | 5432 | shared relational DB (cowork/identity/rag/modelhub — one instance, per-app DBs) |
| qdrant | qdrant/qdrant:v1.12.4 | 6333/6334 | fusion-rag vector DB, tenant-prefix isolation |

Ports respect `architecture/port-registry.yaml` — sidecar middleware uses the
reserved standard ports, none collide with the 53 fusion service ports (11xxx).

### One-command bring-up

```bash
cd fusion-supervisor
cp .env.example .env          # fill FUSION_PG_PASSWORD + FUSION_MLX_API_KEY (required)
bash scripts/prepare-build-ctx.sh   # stage lean build context for business images
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml up -d
```

This brings up traefik + redis + postgres + qdrant + model-hub + cowork, all
healthy. The business overlay (`docker-compose.business.yml`) depends on the
sidecar services (redis/postgres healthy) and connects via shared
`DATABASE_URL`/`REDIS_URL` — no self-hosted Postgres per app (consolidates the
dev-only cowork postgres). The native MLX/gateway attach via
`host.docker.internal:11434` / `:11432`.

### Postgres multi-DB init

The single PG instance creates one DB per app at first boot
(`POSTGRES_MULTIPLE_DATABASES=cowork,identity,rag,modelhub`), via
`scripts/pg-init-multiple-db.sh`. Each app connects by DB name; RLS / per-tenant
isolation remains each app's responsibility.

### Traefik routing

Docker labels on each business service register Traefik routers (TLS on the
`websecure` entrypoint). A static dynamic config (`traefik/dynamic.yml`) routes
`/v1` + `/api/gateway` to the **native** fusion-gateway at
`host.docker.internal:11432`. API-key → tenant resolution stays in
fusion-identity (upstream) — Traefik forwards the key; the gateway stamps
`X-Fusion-Tenant` and rejects invalid keys (auth source-of-truth not duplicated
in compose).

### Sidecar smoke validation

```bash
bash scripts/smoke-sidecar.test.sh      # unit (no Docker): compose presence + guards
bash scripts/smoke-sidecar.sh           # live: 5-step sidecar bring-up + overlay merge
```

Live smoke (design §1.1): precheck → sidecar up → PG multi-DB check → business
overlay on shared PG/Redis → Traefik router table. Pass banner = `SMOKE PASS`.
Auto-teardown via `trap`. Log: `/tmp/fusion-smoke-sidecar.*/smoke.log`.

### Teardown

```bash
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml down --remove-orphans
```

Volumes (`pg_data`, `qdrant_data`, `redis_data`, `traefik_letsencrypt`)
persist across `down` unless `-v` is passed. Nightly backup of PG/Qdrant to
shared NAS is an ops task (§5.4) — `FUSION_BACKUP_TARGET` documented in
`.env.example`.

