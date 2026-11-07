---
doc_id: fusion-supervisor/architecture
product: fusion-supervisor
class: core
entry: service
doc_type: architecture
title: { zh: "fusion-supervisor 架构", en: "fusion-supervisor architecture" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: @dahai
source_of_truth:
  - README.md
  - src/lib.rs
  - src/supervisor/mod.rs
  - src/supervisor/runner.rs
  - src/http_server.rs
---

# 架构 / Architecture

## 总览 / Overview

```
                       ┌─────────────────────────────┐
                       │ architecture/port-registry  │  (manifest, read-only)
                       └──────────────┬──────────────┘
   CLI ──UDS JSON-RPC──┐              │ load_or_empty()
   tray ──HTTP poll────┤      ┌───────▼────────┐   tick loop (probe / crash
   web ──HTTP──────────┴──────│ fusion-sv      │  recovery / backup / retention)
                              │ SupervisorCore │──▶ start.sh (never rewritten)
                              └───────┬────────┘    per-service logs
                          UDS RPC │   │ HTTP :9100
                     /tmp/fusion- │   │ /metrics /health /api/v1/*
                     sv.sock 0600 │   │ (loopback; token for POSTs)
                              ┌───▼───▼───┐
                              │ SQLite    │  health_check / event / boot_pin
                              │ history.db│  7-day retention sweep
                              └───────────┘
```

## 组件与职责 / Components and responsibilities

- **`manifest.rs`** — 读注册表生成服务清单：`supervise: false` 豁免、
  `boot: T0|T1|T2` 分层、`name_to_dir` 注册表键 → 仓库目录映射
  （含 `fusion-code-modenization` 拼写、`business/<name>`、singular/plural
  等显式例外表；子服务键归并到父仓库）。
- **`start_map.rs`** — 启动方式覆盖表：默认 `bash <repo>/start.sh start`，
  例外走 `StartSpec::Daemon`（直接 daemonize 二进制/命令，带 cwd 与 env）
  或 `StartSpec::Script`（嵌套生命周期脚本）。supervisor 从不改写
  `start.sh`——它只编排。
- **`probe_map.rs`** — 每服务探活归一化：HTTP `/health`/`/healthz`、UDS
  ping、TCP 回退；含超时覆盖（如 fusion-mlx 10s）与 restart 回退表
  （无 `restart` 子命令的脚本回退 stop+start）。
- **`supervisor/mod.rs`（SupervisorCore）** — `up_all`/`down_all` 按层并发
  （Semaphore）、`up_one`/`down_one`、tick 循环、drain/rollout、备份调度、
  retention sweep。锁纪律：不持锁跨 sleep/tick，RPC 永不被监督循环饿死。
- **`supervisor/runner.rs`（ServiceRunner）** — 状态机
  `Stopped→Starting→Healthy→Unhealthy→Restarting→Healthy|Failed`，另有
  `Draining` 分支；300s 窗口 5 崩溃上限 + 1→30s 退避；exit 78（EX_CONFIG）
  走 config-fatal 退避 10→60→300s；up 路径启动失败走专用重试曲线
  5→15→60→300s（上限 10 次，不占 crash 窗口）。
- **`env_map.rs`** — 子进程 env 组装：sidecar 连接 env（`DATABASE_URL` /
  `REDIS_URL` / `QDRANT_URL`，按 TCP 可达性注入）、shell 注册 env、
  `FUSION_CLIENT_KEY` 全量注入（三源解析链，全缺拒启动）、
  `strip_env_for` 对 settings.json 单一真源服务剥离 env 覆盖。
- **`http_server.rs`** — 手写 tokio HTTP 面共享同一 core 锁（短锁模式）；
  只读 GET 无 token，编排 POST 与 UDS dispatch 同权同 token。
- **`store.rs` / `alert.rs`** — SQLite 历史（WAL，7 天保留）与告警
  （去重窗口 + 可选 webhook + 事件持久化带 severity）。
- **`compose.rs`** — 可选容器平面编排（默认关闭）；容器只监控告警，
  不参与崩溃重启（Docker restart policy 负责容器自愈）。

## 关键设计决策 / Key design decisions

1. **注册表是唯一真源**：53 个受管服务全部来自
   `architecture/port-registry.yaml`，supervisor 不维护第二份服务清单。
   每个注册项要么可监督要么显式豁免——boot 错误必然是真故障。
2. **编排而非改写**：`start.sh` 是服务作者的生命周期契约，supervisor
   通过 stop/start/restart 动词调用它；例外服务（无 start.sh 或脚本残缺）
   在 `start_map` 显式覆盖并注明上游 issue。
3. **防重启风暴**：404/405（探活路径错）不是崩溃信号，只告警不重启；
   连续 3 次重启型失败才排期重启；操作员 `start.sh` 竞态闸门
   （pgrep + cwd 两阶段检测）避免双启动与行内 env 丢失。
4. **退避不持锁**：崩溃退避记 `restart_after` 截止时间后立即返回，
   `/metrics` 与 RPC 在故障风暴期间保持响应。
5. **进程组隔离**：每个服务子进程独立进程组（`process_group(0)`），
   launchd 重载 daemon 只杀 daemon 不连坐整个托管 fleet；下次 boot
   探活跳过已健康服务零扰动收编。
6. **绑定即边界**：HTTP 面仅 loopback；UDS socket 0600。除
   `FUSION_SV_TOKEN` 外无额外鉴权层。

## 数据流 / Data flow

- **启动**：CLI/tray/web 请求 → UDS/HTTP → `SupervisorCore` →
  `start_map` 决定启动方式 → 子进程（继承 daemon env，含 PATH 增补与
  env 注入/剥离）→ 探活确认 → 状态 + 事件写 SQLite → `/metrics` 更新。
- **崩溃恢复**：tick 探活失败 → fail_streak ≥3 → `restart_after` 排期 →
  到期先查操作员竞态 → `invoke_start` → crash 记账 / 退避升级 /
  `Failed` + 告警。
- **观测**：tray 5s 轮询 `/api/v1/boot`；web 控制台 Services/Health/Alerts
  三页分别走 `/api/v1/status|history|events`；Prometheus 抓 `/metrics`。

## 平台与依赖 / Platform and dependencies

- macOS（Apple Silicon 优先）；launchd LaunchAgent 提供开机自愈
  （daemon plist 携带生产 env，tray plist 从模板生成）。
- Rust 2024 edition；tokio 运行时；rusqlite（bundled SQLite）；
  无 hyper/axum/prometheus crate（HTTP 与 metrics 手写，控制依赖面）。
- workspace 成员：根 crate（`fusion-sv`）+ `tray/`（`fusion-tray`）。
