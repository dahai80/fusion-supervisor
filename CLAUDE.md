# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`fusion-supervisor` is a local-first Rust daemon (`fusion-sv`) that supervises all 41 fusion services registered in `architecture/port-registry.yaml`: start/stop/restart, health probing, crash auto-recovery, health history in SQLite, and alerting. It is the monorepo's *only* persistent supervisor — existing tools (`fusion service status --watch`, `fusion doctor`, `fusion-autotest`, studio `start.sh`) are one-shot or display-only and have no crash restart or history aggregation.

It never rewrites start.sh scripts — it orchestrates them (`bash <repo>/start.sh start|stop|restart`).

## Build / Test / Lint

```bash
cargo build --release              # binary: target/release/fusion-sv
cargo test --all-targets           # ~50 unit + 11 integration (88 test fns total)
cargo test <name> -- --nocapture   # single test by name substring
cargo clippy --all-targets -- -D warnings   # warnings = errors
cargo fmt --all --check
```

Edition 2024. Note: `set_var`/`remove_var` are `unsafe` under edition 2024 — see the `EnvGuard` RAII pattern in `src/config.rs` tests.

## Usage

```bash
fusion-sv daemon          # long-lived supervisor loop + UDS JSON-RPC server
fusion-sv status          # one-shot: list all 41 services + state (over UDS)
fusion-sv top             # live status table, 1s refresh
fusion-sv up [svc]        # start all (layer order: core→platform→app→domain→cluster)
fusion-sv down [svc]      # stop all (reverse layer order)
fusion-sv restart <svc>   # restart one (resets Failed state + crash window)
fusion-sv logs <svc>      # tail last 50 lines of per-service start.sh log
fusion-sv ping            # alive check (prints pong)
```

Config: optional `~/.fusion-sv/config.toml` (all fields have defaults), override path via `FUSION_SV_CONFIG`. Optional auth: set `FUSION_SV_TOKEN` env on daemon; CLI/clients pass `params.token`. History DB at `~/.fusion-sv/history.db` (WAL, 7-day retention). Per-service logs at `~/.fusion-sv/logs/<repo_dir>.log`.

## Architecture

### Dual-mode binary

One binary, two roles. `daemon` = long-lived supervisor tick loop + UDS JSON-RPC server (`/tmp/fusion-sv.sock`, perms 0600). All other subcommands = one-shot RPC client over the same socket. Protocol: JSON-RPC 2.0, one request per newline-terminated line (see `src/rpc/`).

### Daemon lock discipline (critical)

The tick loop (`src/main.rs::run_daemon`) **never holds the `SupervisorCore` Mutex across a sleep**. Pattern: lock → read `poll_interval()` → unlock → `sleep` → lock → `tick_all`. Holding the lock across sleep would block all RPC `status`/`up`/`down` calls. Do not regress this.

`SupervisorCore` is wrapped `Arc<tokio::sync::Mutex<SupervisorCore>>` and shared between the RPC server task and the tick task.

### Source of truth: port-registry.yaml

`manifest::ServiceManifest::load` reads `architecture/port-registry.yaml` for the 41-service manifest. The registry preserves YAML source order via `IndexMap` (NOT `BTreeMap` — BTreeMap's alphabetical key sort would invert core base order, e.g. starting gateway before mlx). `sort_by_layer` then orders: `fixed: true` first, then `Layer` enum order `Core < Platform < App < Domain < Cluster`. Layer assignment (`layer_of`) and repo-dir mapping (`name_to_dir`, handles sub-service keys like `fusion-cowork-ws` → parent `fusion-cowork`, and the `fusion-code-modelization`/`fusion-code-modenization` spelling quirk) live in `src/manifest.rs`.

### Module layout

- `src/main.rs` — CLI (`clap`) entry, daemon tick loop, RPC client wiring
- `src/lib.rs` — module re-exports
- `src/config.rs` — `Config` struct + `~`-expansion (tilde must be expanded, never left literal — see `test_load_expands_tilde_in_db_path`)
- `src/manifest.rs` — registry parse, layer sort, repo-dir mapping
- `src/store.rs` — `HistoryStore`: SQLite wrapped in `std::sync::Mutex` (Connection is `Send` but `!Sync`; the Mutex makes `HistoryStore: Sync` so tick's `&self` across `.await` compiles). Tables `health_check` + `event`, WAL mode. Both tables have a `plane TEXT NOT NULL DEFAULT 'native'` column (issue #7): native services write `plane="native"`, compose containers write `plane="compose"`. `migrate_add_plane` (idempotent `ALTER TABLE ADD COLUMN`, swallows SQLite's "duplicate column name" error) backfills the column on old DBs; existing rows default to `native`. `record_health`/`record_event` take a `plane: &str` arg; `recent_health`/`recent_events` return `HealthRow`/`EventRow` including `plane`
- `src/probe.rs` — three probe kinds (HTTP / UDS / TCP), 2s timeout. `classify_status`: 2xx=Healthy, 404/405=`BadPath`, else `BadStatus`
- `src/probe_map.rs` — service-name → `ProbeSpec` table (known HTTP paths + UDS services); unknown services fall back to TCP port-probe. Also `restart_cmd_for` (`fusion-store` has no `restart` → `stop+start`) and `start_sh_path` (resolves repo dir, honors `FUSION_ROOT`)
- `src/alert.rs` — `Alerter`: `ServiceDown` deduped 5min/service; other kinds always emit. `AlertKind::ContainerStateChange` + `ContainerCrashLoop` (issue #6, compose-plane) always emit; `ContainerCrashLoop` logs at ERROR. Sink = tracing WARN + optional webhook (3-retry, fails back to log only)
- `src/compose_tracker.rs` — pure-logic compose container alert throttle (issue #6): per-container `ContainerRecord` + `observe()` returning `ComposeAlert` decisions (Down/StateChange/CrashLoop/Back); dedup window + consecutive-failure escalation latch. Injected `u64` ts, deterministic tests
- `src/supervisor/mod.rs` — `SupervisorCore`: `up_all`/`down_all` (layered, per-layer concurrency cap via `Semaphore`), `tick_all`, `poll_interval` (switches to `poll_unhealthy_sec` when any runner is down). Production `new()` uses real `bash start.sh` + real probe; `new_with_fns()` injects fakes for tests
- `src/supervisor/runner.rs` — `ServiceRunner` + state machine `Stopped→Starting→Healthy→(Unhealthy)→Restarting→Healthy|Failed`
- `src/supervisor/policy.rs` — `RestartPolicy`: 300s sliding window, 5-crash cap, exponential backoff 1→2→4→8→16→30s. Pure logic with injected `u64` unix-sec timestamps for deterministic tests
- `src/rpc/` — `server.rs` (dispatch), `client.rs`, `schema.rs` (envelope + `Method` enum)
- `src/cli/mod.rs` — `tabled` status rendering

### Crash recovery (the core behavior)

Ports fusion-mlx's `--watchdog`. State machine in `runner.rs::handle_unhealthy`:

- **Anti-restart-storm is the key invariant.** Only `ConnectionRefused` and `Timeout` (`restart_eligible()`) are real crash signals that trigger restart. A `BadPath` (404/405) = probe path wrong, NOT a crash → never restarts, only alerts, never records a crash. The crash-window cap is the final backstop.
- First down transition (`Healthy|Starting → Unhealthy`) only records state + `ServiceDown` alert — does NOT restart yet (avoids flap on transient misjudgment). Restart happens on the *next* tick.
- `Unhealthy|Restarting` + eligible reason → `record_crash` → if cap hit → `Failed` state (stops restart, `RestartCapHit` alert, needs manual `fusion-sv restart <svc>` to reset); else backoff sleep + `invoke_start`.
- Recovery (`Unhealthy|Restarting → Healthy`) emits `ServiceBack` and resets the crash policy.

### Concurrency model

`up_all`/`down_all` bucket services by `Layer` into a `BTreeMap`; layers run serially (core first / cluster last), services *within* a layer run concurrently capped by `max_concurrent_start` (default 4). `down_all` reverses. Up probes first and skips already-healthy services (avoids double-start — important boundary with fusion-gateway's `auto_start` which only starts fusion-mlx).

`real_probe_spec` wraps the async `probe()` in `block_in_place` + `Handle::block_on` because `ServiceRunner.probe_fn` is a sync closure.

## Container multi-node deploy

This repo also holds the containerized multi-node commercial topology (extends `architecture/fusion-deploy-container-multinode-0901.md`). `docker-compose.business.yml` is a **business-plane overlay** — unusable alone, depends on the `fusion-cluster` network created by `fusion-multi-node/docker-compose.yml` (base/cluster plane). Merge to run:

```bash
cd fusion-multi-node
docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d
```

**Containerized:** cluster plane (Master+Agent) + HTTP business plane (model-hub, cowork). **Stays bare-metal:** MLX (`localhost:11434`, Metal/unified memory — reached via `host.docker.internal:11434`) and UDS guardian plane (memory/speech — UDS doesn't cross the container boundary).

The overlay Dockerfiles COPY monorepo-root-relative paths (`requirements.lock`, `fusion-core/`, `fusion-cowork/`), but a root-context build is infeasible (200G+, no root `.dockerignore`). So `scripts/prepare-build-ctx.sh` stages a lean build context into `.build-ctx/` (git-ignored) before building. The model-hub smoke uses a scoped wrapper `scripts/Dockerfile.model-hub-smoke` (adds `build-essential` — upstream `python:3.12-slim` lacks a C++ compiler for sdist deps like miniaudio; tracked in upstream issue).

Smoke validation: `bash scripts/smoke-container-multinode.sh` (5 steps: precheck → cluster up → business up → routed inference → auth handshake). Requires `FUSION_CLUSTER_TOKEN`, `FUSION_MLX_API_KEY`, `FUSION_MULTI_NODE_DIR`, `SMOKE_MODEL` env vars; MLX must be running bare-metal first. Auto-teardown via `trap`.

### Production sidecar (Traefik + Redis + Postgres + Qdrant)

`docker-compose.sidecar.yml` is the OrbStack middleware/data base layer (extends `architecture/fusion-deploy-0902.md` §1.1) for the 10-person Phase 2 production deploy. It creates the `fusion-cluster` network itself (not external), so it can be used standalone as the base instead of the multi-node cluster plane. Bring up with the business overlay:

```bash
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml up -d
```

Fail-closed: requires `FUSION_PG_PASSWORD` + `FUSION_MLX_API_KEY` (see `.env.example`; `.env`/`.env.test` are git-ignored). Single PG instance creates per-app DBs at first boot via `scripts/pg-init-multiple-db.sh` (`POSTGRES_MULTIPLE_DATABASES`). Traefik routes `/v1`+`/api/gateway` to the native gateway (`traefik/dynamic.yml`); Docker labels on business services register TLS routers. Smoke: `bash scripts/smoke-sidecar.sh` (live) + `bash scripts/smoke-sidecar.test.sh` (unit, no Docker). The overlay reuses sidecar redis/postgres (no self-hosted cowork PG in prod).

### Compose plane orchestration (unified bring-up)

The supervisor itself orchestrates BOTH the 41 native services AND the container planes (sidecar + business) in one command — this is the issue #3 integration. `fusion-sv up/down/status/logs` now span both planes. `src/compose.rs` wraps `docker compose --env-file ... -f sidecar -f business ...` behind the `ComposeBackend` trait (prod = `DockerCompose`, test = `FakeCompose` in `compose::test_support`). Disabled by default (`compose.enabled = false` in config → `build_compose` returns `None`, all compose code paths no-op, preserves single-node dev behavior).

- **up_all order**: `up_sidecar()` (traefik/redis/postgres/qdrant, `--wait`) → `up_native()` (layer-ordered, concurrency cap, probe-skip-healthy) → `up_business()` (model-hub/cowork, `--wait`, depends on sidecar + native gateway).
- **down_all order**: `compose down --remove-orphans` (tolerates non-zero) → `down_native()` (reverse layer order).
- **status_all**: native `StatusEntry` rows (plane="native") merged with compose `ps` container rows (plane="compose"). `StatusEntry.state` is now a `String` (was `runner::State`) to hold container states; `StatusEntry.plane` added.
- **Container state mapping** (`ContainerStatus::supervisor_state`): running+healthy→Healthy, exited/dead→Failed, restarting→Restarting, running+unhealthy→Failed, running+no-healthcheck→Healthy, else→Starting.
- **tick_compose**: per-tick `ps()` → each container observed by `compose_tracker` (`src/compose_tracker.rs`, issue #6) which returns alert decisions; `tick_compose` emits an `Alert` per decision AND persists a `plane="compose"` health_check row per container each tick + a `plane="compose"` event row per decision (issue #7: compose containers enter `history.db` alongside native services). `poll_interval` returns `poll_unhealthy_sec` if any container is Failed. Containers are NOT crash-restarted (compose `--wait` + Docker restart policy own that); supervisor only monitors + alerts.
- **compose_tracker (issue #6, alert-side backoff)**: pure-logic throttle (injected `u64` ts, same deterministic-test pattern as `policy.rs`) preventing flapping containers from flooding alerts every tick. Per-container `ContainerRecord` tracks `state`/`raw_state`/`last_alert_ts`/`fail_count`/`escalated`. `observe()` returns 0..2 `ComposeAlert` decisions: `Down` (first failure OR dedup window elapsed OR docker `state` changed), `StateChange` (new docker state during failure), `CrashLoop` (consecutive failures ≥ `container_crash_escalation` config, default 5, emitted once then latched until recovery), `Back` (Failed→Healthy recovery). Alert service key is `compose:{service}` (avoids collision with native service names). `AlertKind::ContainerStateChange` + `ContainerCrashLoop` are NOT deduped (always emit); `ContainerCrashLoop` logs at ERROR level (escalation). `reap(live)` drops snapshots for vanished containers. Config field `container_crash_escalation` (serde default 5).
- **logs dispatch**: RPC `logs` method tries `compose_logs(service)` first (returns `{lines, plane:"compose"}` if service in compose ps), else native tail -n 50 of `~/.fusion-sv/logs/<dir>.log` (plane:"native").
- **Fail-closed**: `check_env()` refuses `up_sidecar` if `FUSION_PG_PASSWORD`/`FUSION_MLX_API_KEY` missing from both env vars and env_file — points to `.env.example`.
- **serde**: docker compose v2 `ps --format json` uses PascalCase fields (`Name/Service/State/Health/Publishers`) and `URL` (all-caps) on publishers — `#[serde(rename_all = "PascalCase")]` + `#[serde(alias = "URL")]` handle both. `normalize_publisher` converts `0.0.0.0:5432:5432/tcp` → `0.0.0.0:5432->5432/tcp`.

To enable: set `compose.enabled = true` in `~/.fusion-sv/config.toml` (paths default to `docker-compose.sidecar.yml` / `docker-compose.business.yml` / `.env` in CWD, all tilde-expanded).

## Boundary with other supervisors

- **fusion-gateway** stays the inference routing entry; its `auto_start` only starts fusion-mlx. Supervisor manages all 41 (including mlx) — double-start avoided by probing-first-then-skip.
- **fusion-studio/Scripts/start.sh** → legacy; supervisor uses registry truth.
- **fusion-cli** `service`/`rag`/`doc` start/stop remain for interactive convenience; supervisor is authoritative.

## Conventions

- Comments are in Chinese (codebase convention); match this.
- 4-space indentation, no docstrings, always include logging (`tracing`).
- Tests inject fakes via `new_with_fns` / `harness` rather than spinning real services — the probe/start functions are `Arc`-ed closures. Deterministic timestamps injected as `u64` unix-sec, never `SystemTime::now()` in `RestartPolicy` logic.
