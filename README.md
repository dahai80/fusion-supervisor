# fusion-supervisor

Local-first fusion service supervisor and monitor. Long-lived Rust daemon `fusion-sv` supervises all 41 fusion services registered in `architecture/port-registry.yaml`: start/stop/restart, health probing, crash auto-recovery (crash-window + exponential backoff), health history in SQLite, and alerting.

## Why

The fusion monorepo had no persistent supervisor. Existing tools are one-shot or display-only: `fusion service status --watch` (6 services, no restart), `fusion doctor` (5 services), `fusion-autotest` (~20, exits on completion), `fusion-studio/Scripts/start.sh` (14, ends after start). 41 services with no continuous watcher, no crash restart, no health history aggregation, no unified liveness path (`/health` vs `/healthz` vs UDS-ping chaos). fusion-supervisor fills this gap.

## Install

```bash
cd fusion-supervisor
cargo build --release   # binary: target/release/fusion-sv
```

## Usage

```bash
fusion-sv daemon          # start supervisor daemon (long-lived)
fusion-sv status          # list all 41 services + state (talks to daemon over UDS)
fusion-sv top             # live status table, 1s refresh
fusion-sv up              # start all services (layer order: core→platform→app→domain→cluster)
fusion-sv down            # stop all (reverse layer order)
fusion-sv restart <svc>   # restart one service (resets Failed state)
fusion-sv logs <svc>      # tail last 50 lines of per-service start.sh log
fusion-sv ping            # ping the daemon (alive check, prints pong)
```

## How it works

- **Source of truth:** reads `architecture/port-registry.yaml` for the 41-service manifest. Never rewrites `start.sh` scripts — orchestrates them (`bash <repo>/start.sh start|stop|restart`).
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

- **fusion-gateway** stays the inference routing entry. Its `auto_start` only starts fusion-mlx; supervisor manages all 41 (including mlx). Double-start avoided: supervisor probes first, skips if already healthy. (Boundary tracked in fusion-gateway issue.)
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
- [ ] `fusion-sv daemon` then `fusion-sv status` lists all 41 services (real registry)
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

## Container Multi-Node Deploy

Containerized multi-node commercial-deployment topology — extends
`architecture/fusion-deploy-container-multinode-0901.md`. Layered Docker
Compose: base cluster plane (`fusion-multi-node/docker-compose.yml`, network
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
   created in `fusion-multi-node`):
   ```bash
   export FUSION_CLUSTER_TOKEN=$(openssl rand -hex 16)
   export FUSION_MLX_API_KEY=fg-admin-key   # matches fusion-mlx api_key
   ```
3. Smoke model cached: `mlx-community-Llama-3.2-1B-Instruct-4bit`
   (download via `HF_MIRROR=https://hf-mirror.com` if missing), available in MLX.
4. Docker Desktop running (v29.4+).
5. Stop any bare-metal `fusion-multi-node` master first (it holds port 11452):
   `cd fusion-multi-node && bash start.sh stop`.

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

```bash
cd fusion-supervisor
export FUSION_CLUSTER_TOKEN=$(openssl rand -hex 16)
export FUSION_MLX_API_KEY=fg-admin-key
export FUSION_MULTI_NODE_DIR=$(cd ../fusion-multi-node && pwd)
export SMOKE_MODEL=mlx-community-Llama-3.2-1B-Instruct-4bit
bash scripts/smoke-container-multinode.sh
```

Runs 5 steps (design §5.1): precheck → cluster up → business up → routed
inference (Master→Agent→`host.docker.internal`→MLX) → auth handshake. Logs to
`/tmp/fusion-smoke.*/smoke.log`. Pass banner = `SMOKE PASS`. Teardown is
automatic via a `trap` (both planes `down --remove-orphans`).

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
(mTLS path): `docker compose ... down` + `rm -rf fusion-multi-node/tls/`.
