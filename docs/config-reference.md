---
doc_id: fusion-supervisor/config-reference
product: fusion-supervisor
class: core
entry: service
doc_type: config-reference
title: { zh: "fusion-supervisor 配置参考", en: "fusion-supervisor configuration reference" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - src/config.rs
  - .env.example
  - README.md
---

# 配置参考 / Configuration reference

fusion-supervisor 读取两类配置：daemon 自身的 `config.toml`（TOML）与
compose/备份平面使用的 `.env`（fork 自 `.env.example`）。所有字段都有默认值，
配置文件只需写覆盖项。

fusion-supervisor reads two kinds of configuration: the daemon's
`config.toml` (TOML) and the `.env` used by the compose/backup plane
(derived from `.env.example`). Every field has a default; write only
overrides.

## 配置文件路径 / Config file locations

| 环境变量 / Env var | 默认 / Default | 说明 / Description |
|---|---|---|
| `FUSION_SV_CONFIG` | `~/.fusion-sv/config.toml` | daemon 主配置；文件不存在时全默认启动（仅 INFO 日志） |
| `FUSION_ROOT` | 无（回退 cwd） | 默认 `start.sh` 路径解析根 + 相对 `env_file` 解析根；实践上必设 |
| `FUSION_SV_TOKEN` | 无 | 设置后 UDS/HTTP 编排操作需 token |
| `RUST_LOG` | `info` | tracing 过滤（未设时默认 info，启动告警可见） |

主配置路径不存在时 daemon 用默认值启动，不会失败。

If the config file is missing the daemon boots with defaults (INFO log only).

## `~/.fusion-sv/config.toml` 字段 / Fields

来源 / Source: `src/config.rs` `Config` struct（serde 默认值即下表）。

| 字段 / Field | 默认 / Default | 说明 / Description |
|---|---|---|
| `socket_path` | `/tmp/fusion-sv.sock` | UDS JSON-RPC 路径 |
| `db_path` | `~/.fusion-sv/history.db` | SQLite 健康历史（WAL） |
| `poll_healthy_sec` | `15` | 全员健康时的探活周期 |
| `poll_unhealthy_sec` | `2` | 有服务异常时的探活周期 |
| `max_concurrent_start` | `4` | 同层并发启动上限（Semaphore） |
| `start_timeout_sec` | `30` | 单次启动超时 |
| `restart_window_sec` | `300` | 崩溃窗口（滑动） |
| `restart_max` | `5` | 窗口内崩溃上限，达上限标 `Failed` |
| `backoff_max_sec` | `30` | 指数退避上限（1→2→4→…→30s） |
| `alert_url` | `""` | 告警 webhook；空 = 仅日志 |
| `alert_dedup_sec` | `300` | 同类告警去重窗口 |
| `container_crash_escalation` | `5` | compose 容器连续失败升级告警阈值 |
| `drain_grace_sec` | `30` | drain 宽限，到期强制 Stopped |
| `rollout_prewarm_timeout_sec` | `60` | rollout 预热探活超时，超时告警并留 Stopped |
| `backup_schedule_sec` | `86400` | 备份调度间隔（daemon tick 检查） |
| `backup_retention` | `7` | 保留最近 N 份备份集 |
| `history_retention_days` | `7` | 健康历史/事件保留天数 |
| `prune_schedule_sec` | `3600` | retention sweep 周期 |
| `registry_path` | `architecture/port-registry.yaml` | 服务清单来源（相对 FUSION_ROOT/cwd 展开 `~`） |
| `metrics_addr` | `127.0.0.1:9100` | HTTP 面监听；`""` = 整个 HTTP 面关闭 |
| `web_dir` | 无 | web 控制台静态目录（如 `~/fusion/fusion-supervisor/web/dist`）；`""` = 不服务 |
| `auto_up_on_start` | `false` | daemon 启动即拉起 boot 集合（T0 ∪ pin=auto） |
| `pg_dump_bin` | 无 | 覆盖 `pg_dumpall` 二进制解析（默认走 PATH） |
| `native_pg_host` | `127.0.0.1` | native 平面备份目标 PG |
| `native_pg_port` | `5432` | 同上 |
| `native_extra_path` | `/opt/homebrew/bin:/usr/local/bin` | 追加到 daemon PATH（仅缺失项，子进程继承）；`""` = 关闭 |

可选 `[compose]` 段（默认 `enabled = false`，native-only）：

| 字段 / Field | 默认 / Default | 说明 / Description |
|---|---|---|
| `enabled` | `false` | true 时 up/down/status/logs 同时覆盖容器平面 |
| `sidecar_file` | `docker-compose.sidecar.yml` | sidecar compose 文件 |
| `business_file` | `docker-compose.business.yml` | business compose 文件 |
| `env_file` | `.env` | 容器平面 env；相对路径优先按 `FUSION_ROOT` 解析 |

示例 / Example:

```toml
metrics_addr = "127.0.0.1:9100"
web_dir = "~/fusion/fusion-supervisor/web/dist"
auto_up_on_start = true

[compose]
enabled = false
```

## `.env`（compose / 备份平面）/ `.env` (compose / backup plane)

从 `.env.example` 复制到仓库根（git-ignored）。占位符是获取指引而非可用密钥，
真实 `FUSION_CLIENT_KEY` 需从 gateway admin API 获取。

Copy `.env.example` to the repo root (git-ignored). Placeholders are
acquisition guidance, not usable keys; obtain the real `FUSION_CLIENT_KEY`
from the gateway admin API.

| 变量 / Var | 必填 / Required | 说明 / Description |
|---|---|---|
| `FUSION_PG_PASSWORD` | compose 启用时必填 | sidecar Postgres 密码；fail-closed，缺失拒启动 |
| `FUSION_CLIENT_KEY` | compose 启用时必填 | gateway 客户端密钥；native 平面由 supervisor 注入每个受管服务（进程 env → `.env` → gateway config.yaml 只读兜底，全缺拒启动） |
| `FUSION_BACKUP_TARGET` | `backup` 用到 | 备份目标（如 NAS 路径）；未设时 `backup` 拒绝执行并告警 |
| `FUSION_PG_USER` / `FUSION_PG_DB` / `FUSION_PG_DATABASES` | 可选 | sidecar PG 账号与附加库 |
| `FUSION_PG_USER`（native 备份） | 可选 | 未设时 `pg_dumpall` 省略 `-U`（brew trust auth） |
| `FUSION_MLX_URL` / `FUSION_GATEWAY_URL` | 可选 | 容器访问宿主 native 核心的地址 |
| `FUSION_DOMAIN` / `FUSION_ACME_EMAIL` | 可选 | Traefik TLS |

约束 / Constraints:

- supervisor 对 `fusion-mlx`、`fusion-health` 子进程主动剥离
  `FUSION_MLX_API_KEY` / `FUSION_CLIENT_KEY`（settings.json 单一真源，
  daemon 环境快照会钉死旧密钥导致全客户端 401）。
- `FUSION_ADMIN_KEY` / `FUSION_GATEWAY_BASE_URL` / `FUSION_TRAINER_BRIDGE_KEY`
  属 shell 注册 env，由 supervisor fail-closed 注入受管的 shell 模块服务
  （前两个有 gateway config 兜底，第三个只认 `.env`）。

- The supervisor strips `FUSION_MLX_API_KEY` / `FUSION_CLIENT_KEY` from
  `fusion-mlx` and `fusion-health` child envs (settings.json is the single
  source of truth; a daemon env snapshot pins stale keys and 401s clients).
- `FUSION_ADMIN_KEY` / `FUSION_GATEWAY_BASE_URL` /
  `FUSION_TRAINER_BRIDGE_KEY` are shell-registration env vars injected
  fail-closed by the supervisor (the first two have gateway-config
  fallbacks; the third reads `.env` only).

## tray 环境变量 / Tray environment variables

| 变量 / Var | 默认 / Default | 说明 / Description |
|---|---|---|
| `FUSION_SV_BASE` | `http://127.0.0.1:9100` | daemon HTTP 面地址 |
| `FUSION_SV_TOKEN` | 无 | 与 daemon 相同 token |
| `FUSION_TRAY_THEME` | 跟随系统 | `dark` / `light` |
