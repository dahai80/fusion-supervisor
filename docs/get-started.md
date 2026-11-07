---
doc_id: fusion-supervisor/get-started
product: fusion-supervisor
class: core
entry: service
doc_type: get-started
title: { zh: "五分钟上手 fusion-supervisor", en: "Get started with fusion-supervisor" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: @dahai
source_of_truth:
  - README.md
  - src/main.rs
  - src/config.rs
---

# 五分钟上手 / Get started in five minutes

本文档中的 `$SV` 指向本仓库 release 二进制（不在 PATH、不在 venv bin）：

`$SV` below is the release binary from this repo (not on PATH, not in a venv bin):

```bash
SV=~/fusion/fusion-supervisor/target/release/fusion-sv
```

## 1. 构建 / Build

```bash
cd ~/fusion/fusion-supervisor
cargo build --release --workspace
# 产物: target/release/fusion-sv (daemon + CLI) 和 target/release/fusion-tray
```

## 2. 启动守护进程 / Start the daemon

必须从 monorepo 根启动，`FUSION_ROOT` 决定默认 `start.sh` 解析路径，
否则 `up` 会 fail-closed 报脚本不存在。子进程继承 daemon 环境——
先激活共享 venv 并 source monorepo `.env`：

Start from the monorepo root: `FUSION_ROOT` decides default `start.sh`
resolution, otherwise `up` fails closed with script-not-found errors.
Children inherit the daemon env, so activate the shared venv and source
the monorepo `.env` first:

```bash
cd ~/fusion
source .venv/bin/activate
set -a && source .env && set +a
FUSION_ROOT=~/fusion $SV daemon
```

可选项：在 `~/.fusion-sv/config.toml` 设 `auto_up_on_start = true`
开机自动拉起 T0 + `pin=auto` 服务（默认 false）。

Optional: set `auto_up_on_start = true` in `~/.fusion-sv/config.toml`
to bring up the boot set (T0 + pin=auto) at daemon start. Default is false.

## 3. 常用命令 / Common commands

```bash
$SV status            # 全量状态表 (native + compose 两平面)
$SV up                # 全量启动 (compose sidecar → native layer 序 → business)
$SV up fusion-mlx     # 单服务启动
$SV restart fusion-mlx
$SV logs fusion-mlx   # 尾部 50 行
$SV top               # 1s 刷新实时表
```

约束 / Constraints:

- `up`/`down` 不带参数作用于全部 53 个服务；带参数为单服务操作。
- `down <svc>` 记录操作来源（pid + argv），手动停止的服务不参与自动恢复。
- 监控读面默认 `127.0.0.1:9100`：`GET /metrics`、`GET /api/v1/status` 无需 token；
  编排类 POST 需要 `FUSION_SV_TOKEN`。

- `up`/`down` without an argument act on all 53 services; with an argument
  they are single-service operations.
- `down <svc>` records the operator origin (pid + argv); manually stopped
  services take no part in auto-recovery.
- The read-only HTTP face defaults to `127.0.0.1:9100`; orchestration POSTs
  require `FUSION_SV_TOKEN`.

## 4. 观测 / Observe

```bash
curl -s http://127.0.0.1:9100/health
curl -s http://127.0.0.1:9100/api/v1/status | head -c 400
```

健康历史落在 `~/.fusion-sv/history.db`（SQLite, WAL），保留 7 天。

Health history lands in `~/.fusion-sv/history.db` (SQLite, WAL), kept 7 days.

## 5. 崩溃自愈验证 / Verify crash recovery

手动杀掉一个非核心服务，30s 内应自动重启并记录历史。300s 窗口内崩溃达
5 次标 `Failed` 并告警 `RestartCapHit`，修复后 `$SV restart <svc>` 复位。

Kill a non-core service: it should self-heal within ~30s. Five crashes inside
the 300s window mark the service `Failed` with a `RestartCapHit` alert;
`$SV restart <svc>` resets it after the fix.

## 6. 守护进程优雅退出 / Stop the daemon

```bash
$SV shutdown    # 经 UDS 优雅退出; 重复调用返回 -32000 (信号只能发一次)
```

## 下一步 / Next steps

- [api-reference.md](api-reference.md) — CLI 与 HTTP 面全量参考
- [config-reference.md](config-reference.md) — `config.toml` 与环境变量
- [troubleshooting.md](troubleshooting.md) — 常见故障与定位
