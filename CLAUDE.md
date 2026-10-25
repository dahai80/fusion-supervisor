# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`fusion-supervisor` is a local-first Rust daemon (`fusion-sv`) that supervises all 53 fusion services registered in `architecture/port-registry.yaml`: start/stop/restart, health probing, crash auto-recovery, health history in SQLite, and alerting. It is the monorepo's *only* persistent supervisor — existing tools (`fusion service status --watch`, `fusion doctor`, `fusion-autotest`, studio `start.sh`) are one-shot or display-only and have no crash restart or history aggregation.

It never rewrites start.sh scripts — it orchestrates them (`bash <repo>/start.sh start|stop|restart`).

## Build / Test / Lint

```bash
cargo build --release              # binary: target/release/fusion-sv
cargo test --all-targets           # 158 test fns total (unit in src/ + 13 files in tests/)
cargo test <name> -- --nocapture   # single test by name substring
cargo clippy --all-targets -- -D warnings   # warnings = errors
cargo fmt --all --check
```

Web console (`web/`, Bun + React + Vite):

```bash
cd web
bun install
bun run dev        # vite on 11473, proxies /api /metrics /health → 9100
bun run build      # tsc --noEmit + vite build → dist/
bun run lint       # biome check src
```

Edition 2024. Note: `set_var`/`remove_var` are `unsafe` under edition 2024 — see the `EnvGuard` RAII pattern in `src/config.rs` tests.

## Usage

```bash
fusion-sv daemon          # long-lived supervisor loop + UDS JSON-RPC server
fusion-sv status          # one-shot: list all 53 services + state (over UDS)
fusion-sv top             # live status table, 1s refresh
fusion-sv up [svc]        # start all (layer order: core→platform→app→domain→cluster)
fusion-sv down [svc]      # stop all (reverse layer order)
fusion-sv restart <svc>   # restart one (resets Failed state + crash window)
fusion-sv logs <svc>      # tail last 50 lines of per-service start.sh log
fusion-sv ping            # alive check (prints pong)
fusion-sv drain [svc]     # drain traffic from one service (or all if no svc) — issue #9
fusion-sv rollout <svc>   # zero-downtime rollout: drain→stop→start→prewarm — issue #9
fusion-sv backup          # one-shot backup: Postgres (pg_dumpall→gzip) + Qdrant → FUSION_BACKUP_TARGET — issue #10
fusion-sv shutdown        # gracefully shut down the daemon over the UDS socket — issue #20
```

The daemon also serves an HTTP face on `metrics_addr` (default `127.0.0.1:9100`): `GET /metrics` (Prometheus), `GET /health`, `GET /api/v1/status|history|events|logs` (read-only, no token), `POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}` (mutations, require `FUSION_SV_TOKEN` as `x-token` or `Authorization: Bearer`). When `web_dir` is configured, the same listener serves the built web console SPA (`web/dist`) at `/` — see `src/http_server.rs`. Embedded mode: fusion-gateway proxies `/shell/fusion-supervisor/` → `:9100` (manifest `web/manifest.json`).

Config: optional `~/.fusion-sv/config.toml` (all fields have defaults), override path via `FUSION_SV_CONFIG`. Optional auth: set `FUSION_SV_TOKEN` env on daemon; CLI/clients pass `params.token`. `web_dir = "<path>/web/dist"` enables static SPA serving (default off). History DB at `~/.fusion-sv/history.db` (WAL, 7-day retention). Per-service logs at `~/.fusion-sv/logs/<repo_dir>.log`.

## Architecture

### Dual-mode binary

One binary, two roles. `daemon` = long-lived supervisor tick loop + UDS JSON-RPC server (`/tmp/fusion-sv.sock`, perms 0600). All other subcommands = one-shot RPC client over the same socket. Protocol: JSON-RPC 2.0, one request per newline-terminated line (see `src/rpc/`).

### Daemon lock discipline (critical)

The tick loop (`src/main.rs::run_daemon`) **never holds the `SupervisorCore` Mutex across a sleep**. Pattern: lock → read `poll_interval()` → unlock → `sleep` → lock → `tick_all`. Holding the lock across sleep would block all RPC `status`/`up`/`down` calls. Do not regress this. Issue #31: this discipline now extends INSIDE `tick_all` — crash-restart backoff no longer sleeps under the lock (`runner.rs` records `restart_after` and returns; the due tick performs `invoke_start`), so `/metrics` + `/api/v1/status` stay sub-ms responsive even while multiple failing services churn.

`SupervisorCore` is wrapped `Arc<tokio::sync::Mutex<SupervisorCore>>` and shared between the RPC server task and the tick task.

### Source of truth: port-registry.yaml

`manifest::ServiceManifest::load` reads `architecture/port-registry.yaml` for the 53-service manifest. The registry preserves YAML source order via `IndexMap` (NOT `BTreeMap` — BTreeMap's alphabetical key sort would invert core base order, e.g. starting gateway before mlx). `sort_by_layer` then orders: `fixed: true` first, then `Layer` enum order `Core < Platform < App < Domain < Cluster`. Layer assignment (`layer_of`) and repo-dir mapping (`name_to_dir`, handles sub-service keys like `fusion-cowork-ws` → parent `fusion-cowork`, and the `fusion-code-modelization`/`fusion-code-modenization` spelling quirk) live in `src/manifest.rs`.

### Module layout

- `src/main.rs` — CLI (`clap`) entry, daemon tick loop, RPC client wiring. Issue #10: `Commands::Backup` → `cli_call(cfg, "backup")`; tick loop 在 `tick_all` 后调 `c.check_backup_schedule(now)` (到期触发 run_backup, fail-closed 已告警)。Issue #20: `run_daemon` 构造 `oneshot::channel<()>`, tx 经 `Arc<Mutex<Option<Sender>>>` (`SharedShutdownTx`) 共享给 UDS + HTTP 两个面, main loop `tokio::select!` 监听 `ctrl_c()` 或 shutdown rx, 任一触发 `tick_handle.abort()` + `http_handle.abort()` + 优雅返回。`Commands::Shutdown` → `cli_call(cfg, "shutdown")`。Issue #22/#26/#25: 第三个 `tokio::spawn` (gated on `cfg.metrics_addr.is_some()`) 跑 `http_server::serve(core.clone(), addr, token, web_dir, shutdown_shared)` — 单一 listener 服务 metrics + API + SPA 静态文件 (见 `src/http_server.rs`)。Issue #29: `cfg.auto_up_on_start=true` 时第四个 `tokio::spawn` 在启动时 lock+up_all 一次 (持锁跨 up_all 与 RPC up 行为一致; gateway Core 层先起, 模块 App 层后注册)。Issue #31: `run_daemon` 开头 FUSION_ROOT 未设且 cwd 无 `architecture/port-registry.yaml` 时一次 WARN (默认 start.sh 路径将 fail-closed); tracing EnvFilter `try_from_default_env` 失败回退 `info` — 默认 error 会吞掉该 WARN
- `src/lib.rs` — module re-exports
- `src/config.rs` — `Config` struct + `~`-expansion (tilde must be expanded, never left literal — see `test_load_expands_tilde_in_db_path`). Issue #9 fields: `drain_grace_sec` (serde default 30 — drain 宽限期, 到期强制 Stopped), `rollout_prewarm_timeout_sec` (serde default 60 — rollout prewarm 超时, 未达 Healthy 则 Stopped + 告警). Issue #10 fields: `backup_schedule_sec` (serde default 86400 — daemon tick 循环检查到期触发 backup), `backup_retention` (serde default 7 — 新备份后裁剪超出 N 份的旧备份). Issue #22 field: `metrics_addr: Option<String>` (serde default `default_metrics_addr` → `Some("127.0.0.1:9100")` — localhost-only 绑定是安全边界, 无 auth); `load()` 中空字符串 trim 后归一化为 `None` (禁用端点)。Issue #25 field: `web_dir: Option<String>` (serde default `None` — 配置 `web/dist` 路径后 HTTP listener 同时服务 web 控制台 SPA)。Issue #29 field: `auto_up_on_start: bool` (serde default false — daemon 启动即 up_all 一次, 探活跳过已健康; 主机重启自愈开关)。Issue #37 fields: `history_retention_days` (serde default 7 — history.db 过期行裁剪天数), `prune_schedule_sec` (serde default 3600 — sweep 调度间隔, backup_due 复用判定)。v0.1.1: **全字段 serde default** (之前部分配置文件 parse 失败 `missing field socket_path`, 与文档 "all fields have defaults" 矛盾); `ComposeConfig` 手动 impl Default (derive 会给 PathBuf 空串默认, 让无 [compose] 段的配置文件拿到空 env_file, 破坏 .env 解析)。Issue #31: `resolve_env_file` — 相对 env_file 在 `FUSION_ROOT` 设置且该处存在时优先相对 monorepo 根解析 (README 引导从根启动 daemon 后, .env 留在 fusion-supervisor/ 会 fail-closed 拒启动 shell-env 服务); 绝对路径/根下不存在回退 CWD
- `src/metrics.rs` — issue #22: Prometheus /metrics 纯逻辑渲染。`render(&SupervisorCore) -> String` 生成 text exposition format v0.0.4, 无 HTTP/无锁/无 DB (调用方 handler 负责 lock 后传引用)。指标: `fusion_sv_up 1` (端点响应即活), `fusion_sv_services_total{plane}` (native=runners.len, compose=若 enabled 则 status_all().filter plane=="compose" count), `fusion_sv_service_healthy{service,port,plane}` (is_healthy: Healthy→1 else 0), `fusion_sv_service_state{...state}` (1 for 当前态, `state_label` 映射 7 个 State 变体全小写), `fusion_sv_crash_count{service}` (`r.policy.crash_count()`, native only). compose 容器无 crash_count (Docker restart policy own)。Issue #26: `render_status_json(&SupervisorCore) -> String` — `/api/v1/status` 只读 JSON 面, 镜像 UDS status 结果 (测试断言两者一致)。5 unit tests
- `src/http_server.rs` — issue #22/#26/#25: supervisor HTTP 面。单一 `tokio::net::TcpListener` (无 hyper/axum), 与 metrics 同一 listener, 绑 `metrics_addr` (默认 `127.0.0.1:9100`, loopback-only 是安全边界)。路由: `GET /metrics` + `GET /health` + 只读 `GET /api/v1/status|history|events|logs` (无 token) + `POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}` (复用 `rpc::server::dispatch_http`, HTTP 凭证 `Authorization: Bearer`/`x-token` 转 `params.token`, 与 UDS 同一校验) + 其余 GET/HEAD → `serve_static` (`web_dir` 配置时服务 SPA, `..`/空段路径穿越 → 404, 含 NUL 字节判二进制拒绝)。请求上限 `MAX_REQUEST_BYTES = 8192`。锁纪律: 短持锁, 绝不跨 await。`shutdown_tx` 与 UDS 共享同一 `SharedShutdownTx`。端到端测试 `tests/test_status_http.rs`
- `src/manifest.rs` — registry parse, layer sort, repo-dir mapping. Issue #31: 条目支持 `supervise: false` (serde default true) — dev-only/k8s artifact/未建条目显式豁免, `ServiceDef.managed=false` 的服务不建 runner (up/down/tick/status 全跳过)。Issue #29: `name_to_dir` parent list also covers `fusion-trainer-bridge`→`fusion-trainer` and `fusion-plugins-ecosystem-console`→`fusion-plugins-ecosystem`; the four shell modules are App layer (gateway Core 层先起, 满足注册顺序)
- `src/start_map.rs` — issue #29: per-service start-command overrides for services whose start differs from `bash <repo>/start.sh`. `StartSpec::Script` (trainer-bridge 自带 scripts/trainer-bridge.sh start/stop) vs `StartSpec::Daemon` (前台 CLI 进程 nohup daemonize: cowork-web `fusion-cowork web serve --port 11461`, plugins console `fusion-plugin-console --port 8795`, issue #31: plugins-ecosystem MCP 主入口 `fusion-plugin-server --transport sse --port 8765` (TCP 探活, 无 /health), issue #33: fusion-store 直跑 `fusion-store/target/release/fs-serve` (deploy/start.sh 上游 BINARY 名 bug, fusion-store#5), fusion-smallbusiness 跑 `fusion-smallbusiness` uvicorn CLI, fusion-simulation 跑 `fusion-sim service start --host 127.0.0.1 --gui` (gui/metrics 口由同进程内嵌, registry 已 supervise:false); issue #34: cowork-ws 跑 `fusion-cowork remote serve`, bench 跑 `fusion-bench serve` (start.sh 引用已删模块, 上游 fusion-bench#27), k12 跑 `fusion-k12 serve` (start.sh 不收 start 子命令, 上游 fusion-k12-teacher#31), code-modenization 跑 `python -m fusion_code_modelization.server` (start.sh 模块无 __main__, 上游 fusion-code-modelization#21); pidfile `~/.fusion-sv/pids/<svc>.pid`, 日志 `~/.fusion-sv/logs/<svc>.log`). `resolve_bin` 优先 `$FUSION_ROOT/.venv/bin/<name>`; 含 `/` 的 bin 相对 FUSION_ROOT 解析 (issue #33). `daemon_start_cmd`/`daemon_stop_cmd` 纯函数构造 shell 命令 (可测). 无覆盖的服务走默认 start.sh 路径。**daemon 必须带共享 venv + 根 .env 启动** (issue #34): 子进程继承 daemon env, 否则 Python 服务 `No module named` 崩、网关鉴权健康检查 401
- `src/store.rs` — `HistoryStore`: SQLite wrapped in `std::sync::Mutex` (Connection is `Send` but `!Sync`; the Mutex makes `HistoryStore: Sync` so tick's `&self` across `.await` compiles). Tables `health_check` + `event`, WAL mode. Both tables have a `plane TEXT NOT NULL DEFAULT 'native'` column (issue #7): native services write `plane="native"`, compose containers write `plane="compose"`. `migrate_add_plane` (idempotent `ALTER TABLE ADD COLUMN`, swallows SQLite's "duplicate column name" error) backfills the column on old DBs; existing rows default to `native`. `record_health`/`record_event` take a `plane: &str` arg; `recent_health`/`recent_events` return `HealthRow`/`EventRow` including `plane`. Issue #25: `recent_events_all(limit)` — 全局事件流, `/api/v1/events` 数据源。Issue #37: event 表加 `severity` 列 (error=容器 crash-loop, info=恢复/drain/backup, warn=其余; `migrate_add_severity` 幂等 ALTER, 旧行默认 warn), `EventRow`/`record_event`/查询全带 severity; `cleanup_old(now, retain_days)` 裁两表过期行 (issue #37 起由 `check_retention` 调度执行, 不再是死代码)
- `src/probe.rs` — three probe kinds (HTTP / UDS / TCP), 2s timeout. `classify_status`: 2xx=Healthy, 404/405=`BadPath`, else `BadStatus`
- `src/probe_map.rs` — service-name → `ProbeSpec` table (known HTTP paths + UDS services); unknown services fall back to TCP port-probe. Issue #29 adds the four shell modules (cowork-web/trainer-bridge/plugins-console/agent-studio, all HTTP /health). Issue #31: fusion-plugins-ecosystem 显式 TCP 8765 (MCP sse 无 /health)。 Issue #33: fusion-store/fusion-smallbusiness HTTP /health, fusion-simulation TCP (gRPC 无 HTTP 面)。 Issue #34: 全 fleet 探活路径对齐上游实况 — model-hub/bench 换鉴权豁免口 `/api/v1/system/health`, health `/api/v1/health`, finance `/api/v1/ready`, k12 `/api/health`, memory 改 HTTP `/healthz` (原 UDS), speech 改 HTTP `/health` (socket 上游已迁), projects 改 UDS (registry 11440 为死值), code/cowork-ws 改 TCP (端点恒 401 / WebSocket, HTTP 探活无意义)。遗留: code-modenization /health 503 (上游健康检查不带 Authorization header, fusion-code-modelization#22), supervisor 正确报 Unhealthy 不重启。 Also `restart_cmd_for` (`fusion-store` → `stop+start`; **issue #29: 任何 `start_map` 覆盖服务也 stop+start** — trainer-bridge 脚本无 restart 子命令, daemonize 进程无 restart 语义) and `start_sh_path` (resolves repo dir, honors `FUSION_ROOT`)
- `src/alert.rs` — `Alerter`: `ServiceDown` deduped 5min/service; other kinds always emit. `AlertKind::ContainerStateChange` + `ContainerCrashLoop` (issue #6, compose-plane) always emit; `ContainerCrashLoop` logs at ERROR. `AlertKind::ServiceDrained` (issue #9, drain 完成) + `RolloutPrewarmFailed` (issue #9, rollout prewarm 超时未达 Healthy) — normal/escalation events. `AlertKind::BackupFailed` (issue #10, 备份 fail-closed: target 未设/不可写/docker exec 失败). Sink = tracing WARN + optional webhook (3-retry, fails back to log only)。Issue #37: `event_sink: Option<EventSink>` — 每次去抖通过真实发出的告警同步写 history.db event 表 (chokepoint 在 `alert()`, native+compose 共用; compose 前缀剥离 + plane=compose 在 `make_event_sink`), `alerts_emitted` 计数 (`fusion_sv_alerts_total`), `severity_of` kind→severity 映射
- `src/backup.rs` — issue #10: NAS 备份原语 `BackupRunner`。injectable `ExecFn = Arc<dyn Fn(&str, &[&str]) -> Result<Vec<u8>>>` (生产=`real_exec` shell `docker compose --env-file ... -f sidecar -f business exec <svc> <args>`, 测试=fake 返回固定 bytes)。`run(now_ts, &HistoryStore, &mut Alerter)` fail-closed: `FUSION_BACKUP_TARGET` (env 优先, 再 `.env` 文件) 未设 → `bail!` + `BackupFailed` 告警; 否则 `pg_dump` (`pg_dumpall -U fusion` → gzip via flate2 `GzEncoder`) + `qdrant_snapshot` (`tar cf - -C /qdrant/storage .` → `storage.tar`) + `prune_old` (按文件名 date 词序降序, 保留 `retention` 份, 删最旧 + 对应 `fusion-qdrant-{date}` 目录) + `record_outcome` (写 history.db `kind=Backup` event)。store/alerter 借用传入 (Connection 不可 Clone, 故 split-borrow)。free fns: `resolve_target()` (env→.env), `backup_due(now, last, schedule)` (纯函数: last==0 恒到期, 否则 `now - last >= schedule`), `date_stamp()` (env `FUSION_BACKUP_DATE` 覆盖便于测试, 否则 `d{day}`)
- `src/compose_tracker.rs` — pure-logic compose container alert throttle (issue #6): per-container `ContainerRecord` + `observe()` returning `ComposeAlert` decisions (Down/StateChange/CrashLoop/Back); dedup window + consecutive-failure escalation latch. Injected `u64` ts, deterministic tests
- `src/env_map.rs` — issue #8: compose enabled 时, 依赖 sidecar 的 native 服务需显式注入连接 env。`required_vars(svc)` maps fusion-identity→[DATABASE_URL], fusion-rag→[QDRANT_URL], fusion-cowork/fusion-model-hub→[DATABASE_URL,REDIS_URL]; `needs_injection(svc)` is the gate. `native_env_for(svc, env_file) -> Result<HashMap<String,String>>` resolves values (process env precedence over `.env`): DATABASE_URL=`postgresql://{FUSION_PG_USER}:{FUSION_PG_PASSWORD}@localhost:5432/{db}` (fail-closed — missing FUSION_PG_PASSWORD → Err), REDIS_URL=`redis://localhost:6379/1`, QDRANT_URL=`http://localhost:6333`. Non-dependent services → empty map (no injection). `pg_db_for` derives per-service PG db name from FUSION_*_DB env (fallback: service name minus `fusion-` prefix)。Issue #29: `shell_env_for(svc, env_file)` — shell 注册 env, **不受 compose 开关控制**: fusion-cowork-web/fusion-plugins-ecosystem-console→[FUSION_SHELL_GATEWAY_URL (兜底 http://127.0.0.1:11432), FUSION_GATEWAY_ADMIN_KEY (兜底只读 fusion-gateway config.yaml web_shell.admin_key, FUSION_GATEWAY_CONFIG 可覆盖路径)], fusion-trainer-bridge→[FUSION_TRAINER_BRIDGE_KEY (无兜底, fail-closed)]。解析顺序: 进程 env → env_file → 兜底
- `src/supervisor/mod.rs` — `SupervisorCore`: `up_all`/`down_all` (layered, per-layer concurrency cap via `Semaphore`), `tick_all`, `poll_interval` (switches to `poll_unhealthy_sec` when any runner is down). Production `new()` uses real start invocation + real probe; `new_with_fns()` injects fakes for tests. **Issue #29: `ArcStartFn` first arg is now the SERVICE NAME (was repo_dir)** — `real_start_pair(svc, cmd, env_overrides)` dispatches via `start_map::start_spec_for(svc)` (override services) else default `bash <name_to_dir(svc)>/start.sh`. `up_native` env 注入两类 (均 fail-closed 拒启动): shell 注册 env (`needs_shell_env`, 无条件, issue #29) + sidecar 连接 env (`compose.enabled && needs_injection`, issue #8)。Crash recovery (`runner.invoke_start`) 的 runner 内部 `StartFn` 闭包在 `new()` 里捕获 svc name + env_file, 重启时解析 shell env (失败仅 ERROR + 空 env 继续 — 服务真起不来由探活告警兜底; 首次启动 up_native 才严格 fail-closed)。`exec_bash_script` 统一脚本执行 (stderr → `~/.fusion-sv/logs/<svc>.log`, 日志名从 repo_dir 改为 svc 名 — 子服务日志不再混入父 repo)。Issue #31: `up_one(name)`/`down_one(name)` — RPC up/down 的 service 级实现 (探活跳过已健康/dep-gate/env 注入 fail-closed/状态机与 up_native 单条目一致)。 Issue #9 drain/rollout primitives: `drain(service, now_ts)` (单服务 begin_drain), `drain_all(now_ts)` (全部, 任一 Err bail), `cancel_drain(service)` (Draining→Healthy), `rollout(service, now_ts, grace, prewarm_timeout)` async (drain→mark Stopped→invoke_start→Starting→prewarm loop 用 `runner.probe_once()` 到 deadline=now_ts+prewarm_timeout; 达 Healthy→return Ok, 超时→Stopped + `RolloutPrewarmFailed` 告警 + bail). **Coordination boundary**: supervisor owns local lifecycle; fusion-gateway owns traffic cut-over; fusion-multi-node owns cluster drain. Issue #10: `run_backup(now_ts)` 构造 `BackupRunner::new(&cfg)` 调 `run(now_ts, &store, &mut alerter)`, 成功则记 `last_backup_ts`; `check_backup_schedule(now_ts)` 用 `backup_due` 判定到期触发 `run_backup` (失败也更新 last_backup_ts 避免每 tick 重试, fail-closed 已告警)。`last_backup_ts: u64` 字段 (0=从未备份 → 首次 tick 即到期)。Issue #37: `store: Arc<HistoryStore>` (Alerter 事件 sink 持克隆), `check_retention(now_ts)` — retention sweep 调度 (语义同 backup 调度, 失败也推进 last_prune_ts 防重试风暴), `events_pruned_total` 累计清除行数 (`fusion_sv_events_pruned_total`); tick_compose 不再显式 record_event, compose 事件统一走 Alerter sink
- `src/supervisor/runner.rs` — `ServiceRunner` + state machine. Issue #31: `restart_after: Option<u64>` — 退避不持锁 sleep, 排期后本 tick 返回, 到点 tick (poll_unhealthy_sec=2s 粒度) 记 crash + `invoke_start`; crash 记账语义不变 (每次实际重启尝试记 1 次) `Stopped→Starting→Healthy→(Unhealthy)→Restarting→Healthy|Failed`, plus issue #9 drain path: `Healthy→Draining→Stopped` (drain 完成) or `Healthy→Draining→Healthy` (drain 取消). `State::Draining` (探活仍跑但 tick 不重启; grace 到期或探活不可达 → `Stopped` + `ServiceDrained` 告警). `drain_grace_until: Option<u64>` field. `begin_drain(grace_sec, now_ts)` (仅 Healthy/Starting/Unhealthy 可 drain), `cancel_drain()` (仅 Draining 可取消 → Healthy), `probe_once()` (单次探活, rollout prewarm 用 — 复用注入的 probe_fn). `handle_unhealthy` 的 `State::Draining` arm 是防御性兜底 (tick 提前返回, 理论不可达), 满足穷尽匹配
- `src/supervisor/policy.rs` — `RestartPolicy`: 300s sliding window, 5-crash cap, exponential backoff 1→2→4→8→16→30s. Pure logic with injected `u64` unix-sec timestamps for deterministic tests
- `src/rpc/` — `server.rs` (dispatch), `client.rs`, `schema.rs` (envelope + `Method` enum). Issue #9: `Method::Drain` (params.service 单服务或无=drain_all) + `Method::Rollout` (params.service 必填)。server.rs Drain 用 SystemTime now_ts; Rollout 先读 `cfg.drain_grace_sec`/`cfg.rollout_prewarm_timeout_sec` 到局部再 `c.rollout(...).await` (避 E0502 读写冲突)。Issue #10: `Method::Backup` (无 params) → `c.run_backup(now_ts)`, 成功 `json!("ok")`, 失败 `-32000` (fail-closed 已告警)。Issue #20: `serve_with_shutdown(core, sock, token, shutdown_tx: Option<oneshot::Sender>)` — Shutdown arm 取出 tx 发送信号 (take 后置 None → 幂等, 重复调用返回 `-32000 shutdown channel unavailable`), 回复 `json!("ok")`。tx 经 `Arc<Mutex<Option<Sender>>>` 共享到每个 `handle_conn` task。`Method::Top` 是客户端命令 (cli_top 循环 cli_status), server arm 返回 `-32601` 带说明 "run `fusion-sv top` directly" (enum 保留, `test_parse_method` 仍断言 SHUTDOWN/TOP 解析)。`serve()` (3 参数) 为向后兼容保留, 内部转调 `serve_with_shutdown(.., None)`。Issue #25: `dispatch_http(method, params, core, token, shutdown_tx)` — HTTP 编排操作复用 UDS dispatch 同一入口 (token 校验语义一致)。Issue #31: **Up/Down 此前忽略 `params.service`** (`up/down <svc>` 实际跑全量 up_all/down_all, `down <svc>` 全停极危险) — 现有 service → `up_one`/`down_one`, 无 → up_all/down_all
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

Multi-node smoke validation was **removed** (PR #24): `scripts/smoke-container-multinode.sh` and the overlay test scripts depended on `fusion-multi-node/docker-compose.yml`, which is not tested on the local single-machine deployment. The `docker-compose.business.yml` overlay itself remains.

### Production sidecar (Traefik + Redis + Postgres + Qdrant)

`docker-compose.sidecar.yml` is the OrbStack middleware/data base layer (extends `architecture/fusion-deploy-0902.md` §1.1) for the 10-person Phase 2 production deploy. It creates the `fusion-cluster` network itself (not external), so it can be used standalone as the base instead of the multi-node cluster plane. Bring up with the business overlay:

```bash
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml up -d
```

Fail-closed: requires `FUSION_PG_PASSWORD` + `FUSION_MLX_API_KEY` (see `.env.example`; `.env`/`.env.test` are git-ignored). Single PG instance creates per-app DBs at first boot via `scripts/pg-init-multiple-db.sh` (`POSTGRES_MULTIPLE_DATABASES`). Traefik routes `/v1`+`/api/gateway` to the native gateway (`traefik/dynamic.yml`); Docker labels on business services register TLS routers. Smoke: `bash scripts/smoke-sidecar.sh` (live) + `bash scripts/smoke-sidecar.test.sh` (unit, no Docker). The overlay reuses sidecar redis/postgres (no self-hosted cowork PG in prod).

### Compose plane orchestration (unified bring-up)

The supervisor itself orchestrates BOTH the 53 native services AND the container planes (sidecar + business) in one command — this is the issue #3 integration. `fusion-sv up/down/status/logs` now span both planes. `src/compose.rs` wraps `docker compose --env-file ... -f sidecar -f business ...` behind the `ComposeBackend` trait (prod = `DockerCompose`, test = `FakeCompose` in `compose::test_support`). Disabled by default (`compose.enabled = false` in config → `build_compose` returns `None`, all compose code paths no-op, preserves single-node dev behavior).

- **up_all order**: `up_sidecar()` (traefik/redis/postgres/qdrant, `--wait`) → `up_native()` (layer-ordered, concurrency cap, probe-skip-healthy, **dep-gate**) → `up_business()` (model-hub/cowork, `--wait`, depends on sidecar + native gateway).
- **dep-gate (issue #5)**: `up_native` explicitly checks that the sidecar containers a native service depends on are `Healthy` before starting it — does NOT rely on layer ordering. The dependency map lives in `src/dep_map.rs` (OnceLock `HashMap`, same pattern as `probe_map.rs`): `fusion-identity→postgres`, `fusion-rag→qdrant`, `fusion-cowork→postgres+redis`, `fusion-model-hub→postgres+redis`. `dep_gate_ok` returns true when compose is disabled (single-node dev preserved) or the service has no mapped deps; on an unhealthy/missing dep it skips startup (marks runner `Stopped`, NOT `Failed` — a dependency problem is not a crash) and emits a `ServiceDown` alert. `ps()` failure degrades to pass-open (avoids false blocking on a flaky compose query).
- **down_all order**: `compose down --remove-orphans` (tolerates non-zero) → `down_native()` (reverse layer order).
- **status_all**: native `StatusEntry` rows (plane="native") merged with compose `ps` container rows (plane="compose"). `StatusEntry.state` is now a `String` (was `runner::State`) to hold container states; `StatusEntry.plane` added.
- **Container state mapping** (`ContainerStatus::supervisor_state`): running+healthy→Healthy, exited/dead→Failed, restarting→Restarting, running+unhealthy→Failed, running+no-healthcheck→Healthy, else→Starting.
- **tick_compose**: per-tick `ps()` → each container observed by `compose_tracker` (`src/compose_tracker.rs`, issue #6) which returns alert decisions; `tick_compose` emits an `Alert` per decision AND persists a `plane="compose"` health_check row per container each tick + a `plane="compose"` event row per decision (issue #7: compose containers enter `history.db` alongside native services). `poll_interval` returns `poll_unhealthy_sec` if any container is Failed. Containers are NOT crash-restarted (compose `--wait` + Docker restart policy own that); supervisor only monitors + alerts.
- **compose_tracker (issue #6, alert-side backoff)**: pure-logic throttle (injected `u64` ts, same deterministic-test pattern as `policy.rs`) preventing flapping containers from flooding alerts every tick. Per-container `ContainerRecord` tracks `state`/`raw_state`/`last_alert_ts`/`fail_count`/`escalated`. `observe()` returns 0..2 `ComposeAlert` decisions: `Down` (first failure OR dedup window elapsed OR docker `state` changed), `StateChange` (new docker state during failure), `CrashLoop` (consecutive failures ≥ `container_crash_escalation` config, default 5, emitted once then latched until recovery), `Back` (Failed→Healthy recovery). Alert service key is `compose:{service}` (avoids collision with native service names). `AlertKind::ContainerStateChange` + `ContainerCrashLoop` are NOT deduped (always emit); `ContainerCrashLoop` logs at ERROR level (escalation). `reap(live)` drops snapshots for vanished containers. Config field `container_crash_escalation` (serde default 5).
- **logs dispatch**: RPC `logs` method tries `compose_logs(service)` first (returns `{lines, plane:"compose"}` if service in compose ps), else native tail -n 50 of `~/.fusion-sv/logs/<dir>.log` (plane:"native").
- **Fail-closed**: `check_env()` refuses `up_sidecar` if `FUSION_PG_PASSWORD`/`FUSION_MLX_API_KEY` missing from both env vars and env_file — points to `.env.example`.
- **serde**: docker compose v2 `ps --format json` uses PascalCase fields (`Name/Service/State/Health/Publishers`) and `URL` (all-caps) on publishers — `#[serde(rename_all = "PascalCase")]` + `#[serde(alias = "URL")]` handle both. `normalize_publisher` converts `0.0.0.0:5432:5432/tcp` → `0.0.0.0:5432->5432/tcp`.

To enable: set `compose.enabled = true` in `~/.fusion-sv/config.toml` (paths default to `docker-compose.sidecar.yml` / `docker-compose.business.yml` / `.env` in CWD, all tilde-expanded).

## Web console (`web/`)

React 18 + Vite 5 + TypeScript + Tailwind SPA (issue #25/#28), served by the daemon's HTTP face when `web_dir` points at `web/dist`. Three pages (hash routes): **Services** (5s poll status table + per-service up/restart/drain/rollout, destructive ops need confirm dialog), **Health** (per-service latency chart from `/api/v1/history`, 10s poll), **Alerts** (event stream from `/api/v1/events`, 10s poll). Shares `@fusion/web-ui` + `@fusion/web-sdk` via `file:vendor/@fusion` — zero registry deps. Dev: `bun run dev` (port 11473, proxies API to 9100; `FUSION_DEV_BACKEND` overrides target). Mutations read token from `VITE_SV_TOKEN` at build time (dev convenience only; embedded mode token injection belongs to the shell host). Gateway wiring: static `[web_shell.modules.fusion-supervisor]` proxy config + `web/manifest.json` (category `infra`).

## Boundary with other supervisors

- **fusion-gateway** stays the inference routing entry; its `auto_start` only starts fusion-mlx. Supervisor manages all 53 (including mlx) — double-start avoided by probing-first-then-skip.
- **fusion-studio/Scripts/start.sh** → legacy; supervisor uses registry truth.
- **fusion-cli** `service`/`rag`/`doc` start/stop remain for interactive convenience; supervisor is authoritative.

## Conventions

- Comments are in Chinese (codebase convention); match this.
- 4-space indentation, no docstrings, always include logging (`tracing`).
- Tests inject fakes via `new_with_fns` / `harness` rather than spinning real services — the probe/start functions are `Arc`-ed closures. Deterministic timestamps injected as `u64` unix-sec, never `SystemTime::now()` in `RestartPolicy` logic.
