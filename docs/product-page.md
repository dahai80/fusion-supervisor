---
doc_id: fusion-supervisor/product-page
product: fusion-supervisor
class: core
entry: service
doc_type: product-page
title: { zh: "fusion-supervisor — 本地服务编排与守护", en: "fusion-supervisor — local service supervision" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - README.md
  - Cargo.toml
  - src/main.rs
---

# fusion-supervisor — 本地服务编排与守护 / Local service supervision

## 它是什么 / What it is

fusion-supervisor 是 fusion monorepo 的常驻守护层：Rust 守护进程 `fusion-sv`
读取 `architecture/port-registry.yaml`（53 个受管服务），对每个服务执行
start/stop/restart、健康探活、崩溃自动恢复（crash 窗口 + 指数退避）、
SQLite 健康历史与告警。它只编排既有的 `start.sh` 生命周期脚本，从不改写它们。

fusion-supervisor is the resident supervision layer of the fusion monorepo.
The Rust daemon `fusion-sv` reads `architecture/port-registry.yaml`
(53 supervised services) and performs start/stop/restart, health probing,
crash auto-recovery (crash window + exponential backoff), SQLite health
history, and alerting. It orchestrates the existing `start.sh` lifecycle
scripts and never rewrites them.

## 解决的问题 / Problem solved

monorepo 此前没有持续监督者：`fusion service status --watch` 只观测 6 个服务且
不重启，`fusion doctor` 只覆盖 5 个，`fusion-studio/Scripts/start.sh` 启动后即退出。
53 个服务没有统一的存活判定（`/health`、`/healthz`、UDS ping 并存）、
没有崩溃重启、没有健康历史。每个消费方（fusion-studio、fusion-cli、各领域服务）
都不得不各自写探活循环、重启逻辑与告警脚本。

The monorepo previously had no continuous supervisor: `fusion service status
--watch` observed 6 services without restarting them, `fusion doctor` covered
5, and `fusion-studio/Scripts/start.sh` exited after startup. 53 services had
no unified liveness path (`/health` vs `/healthz` vs UDS ping), no crash
restart, and no health history. Every consumer had to write its own probe
loop, restart logic, and alerting.

## 消费方不再需要写什么 / What consumers no longer write

- **看门狗循环**：崩溃检测 + 300s 滑动窗口 5 次上限 + 1→30s 指数退避已内建。
  约束：达上限后服务标 `Failed`，需手动 `restart` 复位。
- **探活路径适配**：`probe_map` 为每个服务归一化 HTTP/UDS/TCP 探活。
  约束：新增服务需在注册表登记，否则探活路径可能漂移（404 不重启只告警）。
- **健康历史存储**：SQLite（WAL）`health_check`/`event` 表，7 天保留自动清理。
- **备份脚本**：Postgres `pg_dumpall` + Qdrant 快照到 NAS，每日调度 + 保留份数裁剪。
  约束：`FUSION_BACKUP_TARGET` 未设置时拒绝执行（fail-closed）。
- **零停机发布原语**：drain→stop→start→prewarm 一条命令。
- **开机自愈**：T0/T1/T2 分层 + `pin` 覆盖，宿主机重启后只拉起 boot 集合。
- **监控面板**：Prometheus `/metrics`、只读 JSON status API、web 控制台、桌面 tray。
  约束：HTTP 面默认只绑 `127.0.0.1:9100`，绑定即安全边界，无内置鉴权读面。

- **Watchdog loops**: crash detection + a 300s sliding window with a 5-crash
  cap + 1→30s exponential backoff are built in. Constraint: at the cap the
  service enters `Failed` and needs a manual `restart` to reset.
- **Probe path adapters**: `probe_map` normalizes HTTP/UDS/TCP liveness per
  service. Constraint: new services must be registered; a wrong probe path
  (404) alerts but never restarts.
- **Health history storage**: SQLite (WAL) `health_check`/`event` tables with
  a 7-day retention sweep.
- **Backup scripts**: Postgres `pg_dumpall` + Qdrant snapshot to NAS, daily
  schedule + retention pruning. Constraint: refuses to run when
  `FUSION_BACKUP_TARGET` is unset (fail-closed).
- **Zero-downtime primitives**: drain→stop→start→prewarm in one command.
- **Boot self-healing**: T0/T1/T2 tiers + per-service `pin`; a host reboot
  only brings up the boot set.
- **Monitoring surfaces**: Prometheus `/metrics`, read-only JSON status API,
  web console, desktop tray. Constraint: the HTTP face binds `127.0.0.1:9100`
  only — the binding is the security boundary; read endpoints have no auth.

## 边界 / Boundary

- fusion-gateway 仍是推理路由入口；supervisor 只避免双启（探活跳过已健康）。
- 不改写 `start.sh`；不接管 launchd `KeepAlive` 管理的进程（group stop 够不到）。
- v1 非目标：跨节点聚合、配置热更新、多用户。

- fusion-gateway remains the inference routing entry; the supervisor only
  avoids double-start (probe-first, skip healthy).
- `start.sh` scripts are never rewritten; processes owned by launchd
  `KeepAlive` agents are outside group-stop reach.
- v1 non-goals: cross-node aggregation, config hot-reload, multi-user.

## 组件 / Components

| 组件 / Component | 说明 / Description |
|---|---|
| `fusion-sv` daemon | 监督循环 + UDS JSON-RPC (`/tmp/fusion-sv.sock`) + HTTP 面 |
| `fusion-sv` CLI | 一次性客户端，同一 UDS |
| `fusion-tray` | egui 悬浮点桌面控制面板（分层扇形菜单） |
| web console (`web/`) | React SPA，服务/健康/告警三页，走 HTTP 面 |

## 状态 / Status

- 版本 / Version: 0.1.46（`Cargo.toml`）
- 测试 / Tests: `cargo test --workspace --all-targets`（227 个测试函数）
- 下一步 / Next: see [get-started.md](get-started.md) and
  [api-reference.md](api-reference.md)
