---
doc_id: fusion-supervisor/api-reference
product: fusion-supervisor
class: core
entry: service
doc_type: api-reference
title: { zh: "fusion-supervisor API 参考（CLI + HTTP）", en: "fusion-supervisor API reference (CLI + HTTP)" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - src/main.rs
  - src/http_server.rs
  - src/rpc/schema.rs
  - src/metrics.rs
---

# API 参考 / API reference

fusion-supervisor 有两个程序面：CLI（一次性 UDS JSON-RPC 客户端）与
HTTP 面（`metrics_addr`，默认 `127.0.0.1:9100`）。没有独立的长连接业务
HTTP API；HTTP 面即下述 `/metrics`、`/health` 与 `/api/v1/*`。全部命令
经 `$SV = ~/fusion/fusion-supervisor/target/release/fusion-sv` 调用。

fusion-supervisor exposes two surfaces: the CLI (a one-shot client over the
UDS JSON-RPC socket) and the HTTP face (`metrics_addr`, default
`127.0.0.1:9100`). There is no separate business HTTP API; the HTTP surface
is exactly `/metrics`, `/health` and `/api/v1/*` below. All commands run via
`$SV = ~/fusion/fusion-supervisor/target/release/fusion-sv`.

## CLI 子命令 / CLI subcommands

来源 / Source: `src/main.rs` (`#[derive(Subcommand)]`)。`<svc>` 为必填服务名，
`[svc]` 为可选参数（缺省作用于全部服务）。

```bash
$SV daemon             # 常驻监督循环 + UDS JSON-RPC (/tmp/fusion-sv.sock, 0600)
$SV status [svc]       # 状态表; 带参数为客户端侧过滤, 无匹配时退出码 1
$SV top                # 客户端命令: 1s 清屏循环 status (服务端 RPC 臂返回 -32601)
$SV up [svc]           # 启动: compose sidecar → native layer 序 → compose business
$SV down [svc]         # 停止: compose down → native 反向 layer 序
$SV restart <svc>      # 真替换进程: 复位熔断 → stop → stop_and_verify → start, 返回旧/新 PID
$SV logs <svc>         # 尾部 50 行 (compose 容器日志或 native 服务日志)
$SV ping               # daemon 存活检查, 打印 pong
$SV drain [svc]        # 摘流量: Healthy→Draining→Stopped (宽限 drain_grace_sec)
$SV rollout <svc>      # 零停机发布: drain→stop→start→prewarm (超时告警并留 Stopped)
$SV backup             # 一次性备份: Postgres pg_dumpall→gzip + Qdrant 快照 → FUSION_BACKUP_TARGET
$SV shutdown           # 经 UDS 优雅退出 daemon (幂等: 二次调用 -32000)
```

CLI 退出码 / Exit codes: RPC 错误与 `status` 无匹配 → 1；daemon 不可达 → 3。

鉴权 / Auth: daemon 设置 `FUSION_SV_TOKEN` 后，CLI 经 RPC `params.token`
透传；未设 token 时 UDS 面无鉴权（socket 权限 0600 即边界）。

## HTTP 面 / HTTP face

监听 `metrics_addr`（默认 `127.0.0.1:9100`，仅 loopback）。设
`metrics_addr = ""` 关闭整个 HTTP 面（metrics + status + web 控制台一起关）。

Listens on `metrics_addr` (default `127.0.0.1:9100`, loopback only). Set
`metrics_addr = ""` to disable the whole HTTP face (metrics, status and web
console together).

### 只读端点（无 token）/ Read-only endpoints (no token)

| 方法 / Method | 路径 / Path | 说明 / Description |
|---|---|---|
| GET | `/health` | `{"ok":true}`；lock-free，daemon 忙时也可达 |
| GET | `/metrics` | Prometheus 文本格式 v0.0.4 |
| GET | `/api/v1/status` | 镜像 UDS status：`{"ok":true,"services":[{name,state,port,plane}]}` |
| GET | `/api/v1/history?service=&limit=` | 单服务健康历史（SQLite） |
| GET | `/api/v1/events?limit=` | 全局事件流（告警数据源，含 severity） |
| GET | `/api/v1/logs?service=` | 日志尾部（复用 RPC dispatch） |
| GET | `/api/v1/boot` | 每服务一行 `{name,tier,pinned,state,port}`；tray 唯一轮询面 |

### 编排端点（需 token）/ Orchestration endpoints (token required)

POST `/api/v1/{up,down,restart,drain,rollout,backup,shutdown}` 复用 UDS
dispatch，token 语义与之一致：`Authorization: Bearer <token>` 或 `x-token`
请求头映射到 RPC `token` 参数。token 来自 daemon 进程的 `FUSION_SV_TOKEN`。

POST `/api/v1/{up,down,restart,drain,rollout,backup,shutdown}` reuses the UDS
dispatch unchanged; token semantics are identical — `Authorization: Bearer`
or an `x-token` header maps to the RPC `token` param. The token is the
daemon process's `FUSION_SV_TOKEN`.

```bash
curl -s -X POST http://127.0.0.1:9100/api/v1/up \
  -H "Authorization: Bearer $FUSION_SV_TOKEN" \
  -d '{"service":"fusion-mlx"}'
```

约束 / Constraints: 编排操作与 CLI 完全同权——`down` 全停同样会先打
origin 溯源横幅；`restart` 返回 `{"ok":true,"old_pid":..,"new_pid":..}`；
`shutdown` 幂等语义与 CLI 相同。未知路径 404。

Orchestration POSTs carry the same authority as the CLI: a fleet-wide `down`
still prints the origin banner first; `restart` returns old/new PIDs;
`shutdown` keeps the CLI idempotency semantics. Unknown paths return 404.

### boot 分级端点 / Boot tiering endpoints

| 方法 / Method | 路径 / Path | 请求体 / Body |
|---|---|---|
| POST | `/api/v1/boot/pin` | `{"service":"...","mode":"auto"\|"manual"\|"clear"}`（持久化 SQLite，daemon 重启后保留） |
| POST | `/api/v1/boot/group` | `{"group":"T0"\|"T1"\|"T2","action":"up"\|"down"}`（token + 破坏性操作需确认） |

## Prometheus 指标 / Prometheus metrics

来源 / Source: `src/metrics.rs`。

- `fusion_sv_up 1` — daemon 存活
- `fusion_sv_services_total{plane}` — 按平面计数
- `fusion_sv_service_healthy{service,port,plane}` — 1/0
- `fusion_sv_service_state{service,port,plane,state}` — 当前状态为 1
- `fusion_sv_crash_count{service}` — 滑动窗口崩溃计数（仅 native）
- `fusion_sv_alerts_total` — 去重后发出的告警数
- `fusion_sv_events_pruned_total` — retention sweep 清理的历史行数

## UDS JSON-RPC（内部面）/ UDS JSON-RPC (internal surface)

`/tmp/fusion-sv.sock`（0600），JSON-RPC 2.0 envelope，方法集与 CLI/HTTP
一一映射：`status/top/up/down/restart/logs/ping/drain/rollout/backup/shutdown`
（`top` 无服务端臂）。第三方集成建议优先用 HTTP 只读面 + token 化 POST，
不要直连 UDS（权限与生命周期都跟着 daemon 走）。

`/tmp/fusion-sv.sock` (0600), JSON-RPC 2.0 envelope; the method set maps
one-to-one to the CLI/HTTP commands (`top` has no server arm). Third-party
integrations should prefer the read-only HTTP face plus token-authorized
POSTs instead of the UDS socket.
