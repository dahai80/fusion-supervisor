---
doc_id: fusion-supervisor/troubleshooting
product: fusion-supervisor
class: core
entry: service
doc_type: troubleshooting
title: { zh: "fusion-supervisor 故障排查", en: "fusion-supervisor troubleshooting" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - README.md
  - docs/get-started.md
  - src/main.rs
  - src/supervisor/runner.rs
---

# 故障排查 / Troubleshooting

以下 `$SV = ~/fusion/fusion-supervisor/target/release/fusion-sv`。

## daemon 层 / Daemon issues

### `up` 全部失败：脚本不存在 / `up` fails closed: script not found

**现象 / Symptom**: daemon 日志告警 FUSION_ROOT 未设，`up` 后服务全报
脚本不存在。

**原因 / Cause**: 默认 start 路径解析为 `bash $FUSION_ROOT/<repo>/start.sh`；
未设 `FUSION_ROOT` 时回退 daemon cwd，cwd 不是 monorepo 根即全量失败。

**修复 / Fix**:

```bash
cd ~/fusion && FUSION_ROOT=~/fusion $SV daemon
```

### 手动能跑、开机就挂 / Works manually, dies at boot

**现象 / Symptom**: launchd 拉起的 daemon 下 node/brew 系 start.sh 报
`command -v node` 失败；交互 shell 正常。

**原因 / Cause**: launchd 默认 PATH 不含 `/opt/homebrew/bin`。

**修复 / Fix**: 确认 `config.toml` 未把 `native_extra_path` 设为空串
（默认 `/opt/homebrew/bin:/usr/local/bin`，启动时追加缺失项，子进程继承）。

### CLI 报 daemon 不可达 / CLI reports daemon unreachable

**现象 / Symptom**: 退出码 3，socket 连接失败。

**排查 / Steps**:

```bash
$SV ping                 # 存活检查
ls -l /tmp/fusion-sv.sock
$SV shutdown             # 若怀疑僵死: 优雅退出后重启 daemon
```

socket 权限应为 0600。tray 侧先探 `/api/v1/boot` 再探 `/health`，
`/health` 活而 `/boot` 超时表示 daemon 忙（批量组操作持核心锁数秒），
不是宕机。

### 睡眠唤醒后疑似静默死亡 / Suspected silent death after sleep

**排查 / Steps**: 两轮 tick 间隔远超 `poll_interval` 时 daemon 会打
`time jump detected` WARN；若该行缺席且无 shutdown 日志，可排除正常退出
路径（SIGTERM/ctrl-c/RPC 均留痕）。

## 服务层 / Service issues

### 服务标 `Failed` 不再重启 / Service stuck `Failed`

**原因 / Cause**: 300s 窗口内崩溃达 5 次，或 config-fatal（exit 78）
连续 5 次，或 up 路径启动失败重试达 10 次（约 33-41 分钟）。

**修复 / Fix**: 排查根因（stderr 尾部 20 行附在 `RestartFailed` /
`ServiceConfigFatal` 告警与 `/api/v1/events` 事件里），修复后：

```bash
$SV restart <svc>        # 复位熔断与 Failed 状态
```

### 手动停止被探活翻回 Unhealthy / Manual stop flips back to Unhealthy

`down_one` 标记 `Stopped` 后探活失败不会翻状态（issue #66 修复）；若观察到
翻回，先确认服务是否由自身 LaunchAgent（KeepAlive）或 `nohup` 拉起——
这类进程在 supervisor 的 stop 边界之外。处理：`launchctl bootout` 或
迁移到 supervisor 托管（参考 fusion-comfyui / fusion-gupiao 迁移先例）。

### 重启了但进程没换 / Restart ran but the process did not change

v0.1.46 的 `restart` 走 stop → `stop_and_verify`（20×250ms 确认端口释放）→
start，返回旧/新 PID；若仍观察到 PID 不变，检查服务是否被上游脚本
daemonize 到独立 pidfile 之外的进程。对比：

```bash
$SV restart fusion-cowork-web   # 返回 {"ok":true,"old_pid":..,"new_pid":..}
```

### 探活 Unhealthy 但服务其实活着 / Probe Unhealthy but the service is up

**原因 / Cause**: 探活路径漂移（404/405 = BadPath）或端点鉴权（401）。
BadPath 不触发重启只告警，防重启风暴。

**修复 / Fix**: 在 `probe_map.rs` 校正该服务的探活路径/超时；鉴权端点
（gateway verify_service_key 门后）改走 TCP 探活。已知残留：
fusion-code-modenization `/health` 503 待上游修复，supervisor 如实报
Unhealthy，属预期。

### 401 / backend down 类探活失败 / 401 or backend-down probe failures

**原因 / Cause**: daemon env 快照钉死旧 `FUSION_MLX_API_KEY` /
`FUSION_CLIENT_KEY`（supervisor 已对 fusion-mlx / fusion-health 剥离），
或 shell 注册 env 缺失（fail-closed 拒启动）。

**修复 / Fix**: 确认 monorepo `.env` 与 `fusion-gateway/config.yaml`
中的密钥为当前值；`FUSION_TRAINER_BRIDGE_KEY` 无兜底，只能进 `.env`。

## 备份 / Backup issues

### `backup` 拒绝执行 / `backup` refuses to run

`FUSION_BACKUP_TARGET` 未设时 fail-closed 并发 `BackupFailed` 告警
（安全任务不允许静默跳过）。设好后重试。注意：未配置 target 时 daemon
tick 只打一次性 WARN（功能未启用，非故障）。

### native 平面备份缺库 / Native-plane backup misses databases

默认走 brew Postgres（`127.0.0.1:5432`）`pg_dumpall`。多版本共存时用
`pg_dump_bin` 钉住与 server 匹配的版本；`FUSION_PG_USER` 未设时省略
`-U`（trust auth 默认 OS 用户）。

## 观测 / Observation issues

### `/metrics` 或 `/api/v1/*` 无响应 / No response from the HTTP face

确认 `config.toml` 的 `metrics_addr` 非空（`""` 会关闭整个 HTTP 面）；
默认 `127.0.0.1:9100` 仅 loopback，远程访问需自行加隧道/反代并自行鉴权。

### 历史数据消失 / History rows disappear

7 天保留 sweep（`history_retention_days`，每小时检查）会清理过期行，
属预期；清理量记在 `fusion_sv_events_pruned_total`。

## 定位入口 / Where to look

```bash
$SV logs <svc>                                  # 服务 stdout/stderr 尾部
~/.fusion-sv/logs/<svc>.log                     # per-service 完整日志
~/.fusion-sv/logs/tray.log                      # tray 几何/连接日志
curl -s "http://127.0.0.1:9100/api/v1/events?limit=50"   # 事件流
~/.fusion-sv/history.db                         # health_check / event 表
```
