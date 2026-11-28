---
doc_id: fusion-supervisor/readme
product: fusion-supervisor
class: core
entry: service
doc_type: readme
title: { zh: "fusion-supervisor — 本地服务编排与守护", en: "fusion-supervisor — local service supervision" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - README.md
---

# fusion-supervisor

Local-first 的 fusion 服务监督与守护进程。长驻 Rust 守护进程 `fusion-sv` 监督所有注册在 `architecture/port-registry.yaml` 中的 fusion 服务：启动/停止/重启、健康探活、崩溃自动恢复（crash-window + 指数退避）、SQLite 中的健康历史，以及告警。

> 本文件是 `README.md` 的中文镜像，章节结构、命令、路径、表格与链接与英文版一一对应；如与英文版有出入，以 `README.md` 为准。

## Why

fusion monorepo 此前没有常驻监督进程。现有工具都是一次性的或仅作展示：`fusion service status --watch`（6 个服务，无重启）、`fusion doctor`（5 个服务）、`fusion-autotest`（约 20 个，跑完即退出）、`fusion-studio/Scripts/start.sh`（14 个，启动后即结束）。几十个服务没有持续 watcher、没有崩溃重启、没有健康历史聚合、没有统一的存活判定路径（`/health` vs `/healthz` vs UDS-ping 的混乱）。fusion-supervisor 填补这一空缺。

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

**FUSION_ROOT（实际使用中必填）。** 默认启动路径按 `bash $FUSION_ROOT/<repo>/start.sh` 解析。`FUSION_ROOT` 环境变量优先；未设置时守护进程回退到其 cwd——因此 `cd fusion-supervisor && fusion-sv daemon` 会把所有默认启动路径解析到错误的目录，`up` 以 "脚本不存在" 错误 fail-closed。应改从 monorepo 根启动守护进程：

```bash
cd ~/fusion && FUSION_ROOT=~/fusion fusion-sv daemon
```

当 `FUSION_ROOT` 未设置且 cwd 不像 monorepo 根（缺少 `architecture/port-registry.yaml`）时，守护进程会在启动时输出一行 WARN 日志。

**Registry 豁免（issues #31/#33/#34）。** Registry 条目可设置 `supervise: false` —— supervisor 将完全跳过它们（无 runner、无探活、无重启、不出现在 status 中）：它们是纯开发服务器、内嵌子监听器、k8s 部署产物、或尚未建成、本地无可监督内容的端口。原则（issue #33）：每个 registry 条目要么可监督、要么被显式豁免，因此启动报错总意味着服务真的坏了。已豁免：`fusion-web-shell`（Vite dev；生产环境由网关托管静态 dist）、`fusion-gateway-deploy`（k8s/helm 产物）、`fusion-comfyui-frontend`（Phase 4，未建成），以及 #33 批次——生产路径由网关/守护进程托管静态资源的 Vite dev 控制台（`fusion-supervisor-web`、`fusion-model-hub-web`、`fusion-mlx-web`、`fusion-doc-web-dev`）、无独立进程的内嵌子监听器（`fusion-identity-grpc` = fusion-identity 的进程内 gRPC；`fusion-simulation-gui`/`-metrics` = fusion-sim 的进程内 dashboard/metrics）、尚未建成的（`fusion-autotest-ci-gateway`）——以及 #34 批次：更多 Vite dev 控制台（`fusion-cowork-web-dev`、`fusion-multi-node-web`、`fusion-security-web`、`fusion-bench-web`、`fusion-bench-benchsite`）、内嵌/可选的进程内监听器（`fusion-cowork-sync` = cowork 同步引擎，经 `FUSION_COWORK_CROSS_SYNC=1` 选择性开启；`fusion-multi-node-agent/-mcp/-sync/-mdns/-discovery` 集群平面，worker 平面或在 master 进程内、无默认独立监听器）。#140/#143 批次则直接移除了原先受监督的服务：`fusion-ops`（迁至 VPS systemd，fusion-ops#5）和 `ai-assistant-agent`（自愈型应用——其自身调度器有意每约 5 分钟以全新 PID 重启它；监督它会将每次自愈误判为崩溃并触发告警风暴，见 issue #142）。**自循环型业务应用不得被监督**：应注册为 `supervise: false`，把生命周期交给它们自己的工具链。

**无 start.sh 的受监督服务（issues #33/#34）。** 真实启动路径与 `bash <repo>/start.sh` 不同的服务经 `start_map` 覆盖接线：`fusion-store`（直接守护化 `fusion-store/target/release/fs-serve`——`deploy/start.sh` 有上游二进制名 bug，fusion-store#5）、`fusion-smallbusiness`（守护化 `fusion-smallbusiness` uvicorn CLI）、`fusion-simulation`（守护化 `fusion-sim service start --host 127.0.0.1 --gui`；gRPC 经 TCP 探活，无 HTTP 面）、`fusion-plugins-ecosystem`（:8765，守护化为 `fusion-plugin-server --transport sse --port 8765`，TCP 探活；MCP 服务器无 `/health`），外加 #34 批次上游 start.sh 已损坏的服务——`fusion-cowork-ws`（`fusion-cowork remote serve`；WebSocket，TCP 探活）、`fusion-bench`（`fusion-bench serve`；start.sh 引用了已删除模块，fusion-bench#27）、`fusion-k12-teacher`（`fusion-k12 serve`；start.sh 不接受 `start` 子命令，fusion-k12-teacher#31）、`fusion-code-modenization`（`python -m fusion_code_modelization.server`；start.sh 导入的模块无 `__main__`，fusion-code-modelization#21）。全部经过活体验证。

**嵌套生命周期脚本（issue #81）。** `fusion-office-api`（:11492）是另一类覆盖：其仓库根 `start.sh` 只管理 `fusion-office` shell（:11485，`node scripts/shell-serve.mjs`），而 API 自身的生命周期脚本嵌套在 `fusion-office/services/office-api/start.sh`（一个正规的 `start|stop|restart|status|log` 脚本，带 pidfile 和 `/healthz` 等待）。supervisor 经 `StartSpec::Script` 条目编排该嵌套脚本（trainer-bridge 先例），对 11492 探 HTTP `/healthz`，并在 `name_to_dir` 中把 registry key 映射到父仓库目录（`fusion-office-api` → `fusion-office`；默认映射指向了不存在的 `fusion-office-api/` 目录）。重启 API 不会扰动 11485–11489 的 shell 监听器——独立 pidfile、独立进程。

**直接采用 start.sh（issue #89）。** `fusion-design`（:11493，`fd-server` 二进制，dahai80/fusion-design#32）无需 `start_map` 覆盖——其 `start.sh` 是正规的 `start|stop|status|log` 生命周期脚本，带 pidfile（`.server.pid`）和 SIGKILL 兜底。两处探活修正：健康端点是 `GET /api/health`（router.rs；`deep=1` 查询是重型自检，不用于存活判定），且 `restart` 回退为 stop+start（脚本无 `restart` 子命令）。

**探活路径对齐（issue #34）。** 全 fleet 探活路径漂移在 `probe_map` 中修复：`fusion-model-hub`/`fusion-bench` 探鉴权豁免口 `/api/v1/system/health`，`fusion-health` 探 `/api/v1/health`，`fusion-finance` 探 `/api/v1/ready`，`fusion-k12-teacher` 探 `/api/health`，`fusion-memory` 改 HTTP `/healthz`（原 UDS），`fusion-speech` 改 HTTP `/health`（socket 已在上游迁移），`fusion-projects` 改 UDS（registry 端口 11440 是死值），`fusion-code`/`fusion-cowork-ws` 改 TCP（端点有鉴权门 / WebSocket——HTTP 探活会永远 401）。已知遗留：`fusion-code-modenization` 的 /health 在上游修复其网关检查前持续 503（请求不带 Authorization header，fusion-code-modelization#22）——supervisor 正确报告 Unhealthy，不做重启扰动。

**守护进程环境（issue #34）。** 子进程继承守护进程的环境。启动守护进程时须激活共享 venv 并 source monorepo 根的 `.env`，否则 Python 服务会以 `No module named ...` 崩溃循环、走网关鉴权的健康检查会 401：

```bash
cd ~/fusion && source .venv/bin/activate && set -a && source .env && set +a && FUSION_ROOT=~/fusion fusion-sv daemon
```

**守护进程 PATH 增强（issue #80）。** launchd 默认 `PATH`（`/usr/bin:/bin:/usr/sbin:/sbin`）不含 `/opt/homebrew/bin`，因此凡是经 `command -v` 解析 `node`/Homebrew 二进制的受监督 `start.sh`，在守护进程由 launchd 拉起时都会失败（交互式 shell 中正常——典型的"手动能跑、开机就挂"分裂）。守护进程启动时，`native_extra_path`（默认 `/opt/homebrew/bin:/usr/local/bin`，空串禁用）中当前 `PATH` 缺失的条目会被追加到守护进程自身的 `PATH`——只追加，不改变既有解析顺序；交互式启动（已包含这些目录）是无操作。所有子进程 exec 路径都继承守护进程 env，因此这一个注入点即可覆盖 `exec_bash_script`、`StartSpec::Daemon` exec 和崩溃恢复重启。合并逻辑是纯函数 `config::append_path_entries`（保序去重，有单元测试）。

**settings 持有型服务的 env 剥离（issues #39/#40/#45）。** 继承是把双刃剑：`fusion-mlx` 的 `start.sh` 让 `FUSION_MLX_API_KEY` 环境变量优先于 `~/.fusion-mlx/settings.json` 的 `auth.api_key`，因此携带 monorepo `.env` 中陈旧 key 的守护进程会把那个快照钉进每一次 supervisor 触发的重启——所有 settings.json 客户端（网关后端、爬虫）全部 401，而服务探活仍显示 Healthy。supervisor 现在会从 `fusion-mlx` 子进程 env 中剥离 `FUSION_MLX_API_KEY`（`env_map::strip_env_for`），让每次启动都从 settings.json 现场解析 key——单一真源。Issue #45 把同样的剥离扩展到 `fusion-health`（其 `config.py` 同样让环境变量优先于 settings.json——守护进程的陈旧快照使 backend 探活 401、`/api/v1/health` 报 503 degraded）。覆盖所有 spawn 路径：`up_all`、`up_one` 和崩溃恢复重启。compose 的 `check_env` 门仍要求守护进程自身持有该变量（它 gate 的是真正需要它的 compose 容器）。

**Sidecar 连接 env 与 compose 开关解耦（issue #78）。** `env_map::native_env_for` 为需要 sidecar 中间件的服务构建 `DATABASE_URL`/`REDIS_URL`/`QDRANT_URL`（`fusion-identity`→Postgres，`fusion-rag`→Qdrant，`fusion-cowork`/`fusion-model-hub`→Postgres+Redis）。此注入过去被 `compose.enabled` 门控——compose 关闭（默认态）时，supervisor 启动的 `fusion-cowork` 会静默回退 SQLite、`fusion-rag` 回退 `:memory:`，在生产数据丢失的同时一直显示 Healthy。现在注入发生在所有 spawn 路径（`up_all`、`up_one`、崩溃重启），与 compose 开关无关。Postgres 解析是平面感知的：compose 平面上 `FUSION_PG_PASSWORD` 保持 fail-closed（缺失 → 拒绝启动——sidecar PG 使用密码认证，静默 trust URL 会连到错误的库）。native 平面上（brew PG，按 `fusion-db-conventions-1030` §1，trust 认证，不存在密码）密码缺失是正常的：若 `127.0.0.1:5432` TCP 可达（300ms 探测），supervisor 注入免密码的 `DATABASE_URL`（除非设置了 `FUSION_PG_USER`，否则省略 user 段——trust 认证下 libpq/asyncpg 默认用 OS 用户）；不可达 → 跳过该 key 并记 INFO 日志，让应用回退 SQLite（空 map 仅为这类跳过而允许）。Qdrant 同理：注入 `QDRANT_URL` 前先探测 `127.0.0.1:6333` 的 TCP 可达性（300ms）；不可达 → 跳过该 key，让 fusion-rag 使用其 `:memory:` 回退。

## 业务平面接管（issue #45）

四个业务服务注册在 `architecture/port-registry.yaml` 中但不带 `fusion-` 前缀，仓库位于 monorepo 的 `business/<name>/`：

| Service | Port | Start command (start_map Daemon spec) | Probe |
|---|---|---|---|
| ai-family | 11483 | `bash business/ai-family/start.sh start` (default path; proper start/stop/status case statement) | TCP (every router is `verify_service_key`-gated — HTTP would 401 forever) |
| ai-fit | 11510 | daemonized `uvicorn server.main:app --port 11510`, cwd `business/ai-fit` (issue #55: registry key and dir both renamed `fit-ai`→`ai-fit`; dropped the broken `--app-dir server` — uvicorn imports from repo root) | HTTP `/api/v1/system/health` (auth-exempt) |
| ai-speech-exam | 11560 | daemonized `uvicorn app.main:app --app-dir backend --port 11560`, cwd `business/ai-speech-exam`, env `FUSION_AI_SPEECH_EXAM_CONFIG=config/config.yaml` | HTTP `/health` (unauthenticated) |
| ai-apparelmockup | 11520 | daemonized `uvicorn server.app:app --port 11520`, cwd `business/ai-apparelmockup` (issue #55: upstream switched from static http.server to FastAPI) | TCP (no public health route observed — HTTP could 404 = BadPath forever) |

Issue #55 将业务平面扩展到 2026-10-27/28 端口治理批次（全部 `StartSpec::Daemon`，上游默认值与 registry 仍不一致的钉死为 registry 端口）：

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

三个 registry 条目在上游阻塞解除前被显式豁免（`supervise: false`）：`ai-ads-videobuilder-web-dev`（纯 dev 的 vite 服务器）、`envipermit`（容器化设计；其部署 compose 把 API 映射到 11510，与 ai-fit 撞口）、`ai-surlsch00l-bookingagent`（`surl-agent serve` 硬编码 11570 ≠ registry 11507；迁移 TASK 等上游推进，无 git remote 可提 issue）。

三个风险点决定了设计：

1. **仓库目录映射**：`manifest::name_to_dir` 把 registry key 映射到 `business/<name>`（默认 `~/fusion/<name>` 不存在——所有启动路径都会 fail-closed）。
2. **无参数 start.sh**：`ai-speech-exam`（以及以前的 `fit-ai`/`ai-apparelmockup`）的 start.sh 会 `exec` 一个前台进程且忽略子命令参数——`bash start.sh stop` 会**启动**服务。它们经 `StartSpec::Daemon` 接管（issue #34 模式），该模式现在支持 `cwd`（相对 `FUSION_ROOT` 解析）和应用内部 `env`。Daemon pid 纪律：`mkdir`/`cd ... || exit 1`/`export` 用 `;` 连接（绝不用 `&&`——`A && B &` 会把整条列表后台化，pidfile 里存的是 subshell pid，stop 时孤儿化服务）。
3. **FUSION_CLIENT_KEY 注入（反转默认，issue #85）：** 注入到每一个受监督的 native 服务的每条 spawn 路径（up_all/up_one/崩溃重启）；解析顺序：进程 env → `.env` → 只读回退到 `fusion-gateway/config.yaml` 的 `auth.api_keys`（优先 `name=admin` 条目）。三个来源全缺 → supervisor 拒绝启动该服务（fail-closed），而不是 spawn 一个注定崩溃循环的进程。为何反转默认：旧的手工维护 allowlist 连续三次漏登 key（fusion-rag——其 `/ask` LLM 通道回退到 daemon env 快照中已轮换失效的 `FUSION_MLX_API_KEY` 别名，`/health` 记录 `llm_auth=rejected`；ai-trysuite #84；fusion-code-modenization #85——`/health` 网关检查 401，探活 Unhealthy）——每个新的网关消费服务都静默降级，直到有人发现。不读该环境变量的服务会忽略它，注入无副作用；fail-fast 的 `:?` 消费者则由它满足。刻意排除：`fusion-gateway`（上游 config.go 刻意不 BindEnv `FUSION_CLIENT_KEY`——环境中的 client key 可能经 `api_key_env` 覆盖其 mlx backend key，fusion-gateway#412）、`fusion-mlx`（推理服务器；认证单一真源是 `settings.json`，与 `strip_env_for` 同理）、纯静态托管（`customer-insight` ×2、`ai-apparelmockup-web`——不打网关）和 `fusion-health`（issue #88：在 `:11434` 直连 mlx，其 v1.2.3 `_mlx_key()` 让 `FUSION_CLIENT_KEY` 优先于 settings.json 解析——client key 不是有效的 mlx 认证，注入使它的 backend 检查永久 401 / 503-degraded；同时在 `strip_env_for` 中剥离作双保险，上游 fusion-health#32）。

重启语义：`ai-family`/`ai-trysuite`/`fusion-design` 的 start.sh 无 `restart` 子命令，Daemon-spec 服务根本没有重启语义——`restart` 回退为 stop+start（`probe_map::restart_cmd_for`）。

部署注意：`ai-speech-exam` 读取 `config/config.yaml`（git 忽略；仅提交了不含机密的 `config.example.yaml`）。复制示例文件是认可的部署步骤。

**Web 控制台 vendored SDK 重新同步（issue #43）。** `web/vendor/@fusion/web-sdk` 从 `fusion-web-shell` tag **v0.1.2** 重新 vendored：吸收了 host-bridge 的 `isHttpErrorResult()` 类型谓词修复（裸字符串 bridge 结果不再抛 `TypeError: Cannot use 'in' operator`，上游 #75）和由后台 host 拉取播种的缓存式 `theme.get()`（上游 #77）。本地的 `?embedded=1` 无父帧补丁（根级深链）在其上重新套用——上游从未携带该补丁。

当 `compose.enabled = true` 时，`up`/`down`/`status`/`logs` 一条命令同时覆盖全部 native 服务和容器平面（Traefik+Redis+Postgres+Qdrant sidecar + model-hub/cowork 业务）——真正的单命令拉起加统一监控。默认关闭（仅 native，保持单节点 dev 行为）。

## 工作原理

- **真源（source of truth）：** 读取 `architecture/port-registry.yaml` 获取服务清单。从不改写 `start.sh` 脚本——只编排它们（`bash <repo>/start.sh start|stop|restart`）。
- **Registry key → 仓库目录映射（issue #57）：** `name_to_dir` 把 registry key 映射到本地仓库目录。大多数 key 与目录同名；已知例外放在显式表里（`fusion-code-modelization` → `fusion-code-modenization` 拼写差异、业务平面 key → `business/<name>`、以及 `fusion-multi-node` → `fusion-multi-nodes`——registry key 是单数而磁盘上的仓库目录是复数；#57 之前默认映射指向不存在的目录，Cluster master 每次 boot 都 fail-closed；以及 `fusion-model-hub` → `fusion-models-hub`——同类单复数问题，2026-10-31 修复：仓库在磁盘上改名但 registry 未改，默认映射指向不存在的 `fusion-model-hub/`，该服务此前只能手工启动）。子服务 key（`fusion-cowork-ws`、`fusion-multi-node-agent` 等）经 `(prefix, dir)` 表解析到父仓库，因此复数目录的父服务也能正确解析。
- **双模二进制：** `daemon` = 长驻监督循环 + UDS JSON-RPC 服务器（`/tmp/fusion-sv.sock`，权限 0600）；CLI 子命令 = 走同一 UDS 的一次性客户端。
- **健康探活：** 每服务 `probe_map` 归一化存活路径（HTTP `/health`/`/healthz`、UDS `ping`→`pong`、TCP 回退）。默认 2s 超时；可经 `ProbeSpec.timeout` 按服务覆盖（issue #79：`fusion-mlx` 探 `/healthz` 用 10s——GIL 重负载下一个健康但繁忙的应答可能超过 2s，旧的全局超时会把它升级成破坏性重启、杀掉在途推理流量）。
- **崩溃恢复：** 移植 fusion-mlx 的 `--watchdog`——300s 滑动窗口、5 次崩溃上限、指数退避 1→2→4→8→16→30s。退避是排期的，不在锁内 sleep（issue #31）：tick 记录 `restart_after` 截止时刻即返回，因此失败服务重试期间 `/metrics` 和 status RPC 保持响应。达到上限 → `Failed` 状态，停止重启并告警（手工 `fusion-sv restart <svc>` 复位）。`up [svc]` / `down [svc]` 端到端都是真正的单服务操作（issue #31 修了 RPC 侧——`up_one`/`down_one`；issue #41 修了 CLI 侧，它解析了 `[SERVICE]` 参数却静默丢弃，导致 `down <svc>` 停掉整个 fleet——参数现在经测试过的 `cli::up_down_target` 辅助函数路由）。
- **防重启风暴：** 404/405（`BadPath`）= 探活路径错误，不是崩溃信号 → 永不重启，只告警。只有 `ConnectionRefused`/`Timeout` 触发重启。崩溃窗口上限是最终兜底。连续失败门（issue #79）：可重启的探活失败递增 `fail_streak`；连续 3 次（2s 不健康轮询下约 6s）才排期一次重启。任何一次 Healthy 探活都会清零 streak 并清除陈旧的 `restart_after` 排期——探活延迟抖动的繁忙服务不会升级为重启，而真死仍被及时恢复。Timeout/refused 区分（issue #153）：探活 `Timeout` 意为"慢或暂时不可达"，不是"死了"（死进程答 `ConnectionRefused`），因此超时需要连续 15 次失败（`TIMEOUT_RESTART_STREAK`，按 mlx 10s 探活超时是分钟级）才排期重启——2026-11-24，一个健康的 fusion-mlx 在持续 27B decode 中 GIL 打满，`/healthz` 反复超时，3-streak 一天里三次在生成中途杀掉引擎。`ConnectionRefused`/`ProcessExit` 保持 3 快 streak，启动重试路径（进程尚未监听属预期）也保持快阈值。
- **操作员重启竞态护栏（issue #86）：** 端口探活无法区分"操作员的 `start.sh restart` 正处于 do_stop 窗口"与真崩溃——两者都是 `ConnectionRefused`。与操作员抢跑曾会双重拉起服务，且静默丢掉操作员的行内 env（supervisor 的 re-exec 只带 daemon env 快照，`env_n=0`——2026-10-31 fusion-mlx 的 `FUSION_MLX_HEAVY_ADMISSION_LINE=0.99` 实证丢失）。在到期的自动重启触发前，runner 检查 `pgrep -f "<repo>/start[.]sh"`（括号写法防 pgrep 兄弟进程自匹配）：若操作员脚本进程存活，本 tick 跳过重启——不记崩溃（操作员动作不是崩溃）、保留 `restart_after` 排期，并以 WARN（每次故障只发一次，恢复前锁存）说明行内 env 注意事项。操作员脚本退出后，探活要么自行恢复（自愈，操作员 env 保留），要么下一个到期 tick 正常重启。supervisor 自身的 exec 在核心锁内同步执行，永远不会自匹配。v0.1.27 堵上了一个活体确认的缺口：操作员在仓库目录内执行 `./start.sh restart` 时 cmdline 不含 repo 前缀，phase-1 模式漏检（2026-10-31 实证，supervisor 在 10:42 以 `env_n=0` 抢跑了一次相对路径的操作员重启）。检测现为两阶段：phase 1 匹配 cmdline 中的 `<repo>/start.sh`；phase 2 抓取所有 cmdline 仅含 `start.sh` 的进程，用 `lsof -a -p <pid> -d cwd` 读其工作目录，仅当 cwd 解析到 `FUSION_ROOT/<repo_dir>` 时才认定为操作员。括号 `[.]` 写法仍然防 pgrep 兄弟自匹配，且 supervisor 自身 exec 的脚本以 daemon cwd 运行，phase 2 也不会匹配到自己。
- **配置致命退避 + 快速失败熔断（issue #47）：** 当启动尝试以退出码 78 结束（`EX_CONFIG`——fusion-gateway 的"配置损坏，重启无用"契约），supervisor 停止硬敲：退避升级 10s→60s→300s（替代正常的 1→2→4 快序列），触发携带子进程 stderr 尾部的 `ServiceConfigFatal` 告警，连续 5 次配置致命退出标记服务 `Failed`（修好配置后手工 `fusion-sv restart <svc>` 复位）。独立地，一个快速失败熔断器盯着每个服务：启动成功后 10s 内又崩溃、连续 3 次，触发 `FastFailLoop` 告警并将该服务切换到同样的升级退避。两者在恢复 Healthy 时复位。失败原因浮出：启动失败会捕获子进程最后 20 行 stderr，因此 `RestartFailed`/`ServiceConfigFatal` 告警详情——以及 `/api/v1/events` 背后的 history.db 事件行——携带真正的根因，而不只是退出状态。
- **启动失败重试而非终态 Failed（issue #90）：** 由 `up` 路径发起、失败的启动不再被停进终态 `Failed`。开机依赖竞态（brew Postgres 还在 `initdb`、陈旧的 `postmaster.pid`、gateway 等 `:5432`）曾让 `fusion-gateway` 和 `fusion-identity` 死到操作员介入为止——崩溃恢复只从 `Healthy`→down 的转移触发，从未起来的服务没有恢复路径（2026-11-02 boot：gateway 宕 13.5 分钟，identity 38 分钟）。现在启动失败把 runner 置为 `Unhealthy` 并排期重试（`restart_after`），tick 循环沿专用曲线 5s→15s→60s→300s 重试（`start_retry_delay`——前两次快速追赶把开机竞态死窗压缩到秒级，因为依赖竞态通常一分钟内自愈；后面 60s/300s 步进避免硬敲真正损坏的 start.sh），上限 `START_FAIL_CAP=10` 次（约 33 分钟），之后标记 `Failed` 并告警 `RestartCapHit`，detail 为 `start-fail cap N hit`。这些重试刻意不占崩溃窗口名额（`policy.crash_count` 不动——等依赖不是崩溃循环）。排除项：退出码 78（`EX_CONFIG`）仍立即失败（配置致命，重试无用）；env 解析 fail-closed 错误仍标记 `Failed`（缺 key 不会自己好）；手工启动的服务保持正常崩溃语义——重试流由仅 `up` 路径设置的显式 `start_retry` 标志门控，而非"从未被 supervisor 启动过"。启动成功、恢复 `Healthy`、`down <svc>` 或手工 `fusion-sv restart` 都会清掉排期和 streak。`boot/group` 汇总新增 `retrying` 计数，让 API 客户端能区分"排期重试中"与"终态失败"。
- **菜单常驻后端的 boot 层级晋升（issue #91）：** `architecture/port-registry.yaml` 条目带 `boot: T0|T1|T2` 字段（默认 T2 = 按需；守护进程在启动时读取，因此改 tier 需要重启守护进程才生效）。`fusion-trainer-bridge`、`fusion-plugins-ecosystem`、`fusion-plugins-ecosystem-console` 晋升 T2→T1：三者支撑 shell 菜单中始终期望存在的入口（Trainer 控制台、能力中心），而 T2 服务永远不会自启——fleet 重启后它们的动态 shell 注册蒸发、菜单消失。菜单分层（MT-D）让*已注册*条目在停机时保持可见，但救不了从未注册的服务。回归测试 `test_boot_tier_parse_default_and_explicit` 钉死解析行为（显式 T1 生效，字段缺省 = T2）。
- **Shell 菜单 manifest 对齐（issue #92）：** `web/manifest.json` 自声明更新为已批准的分类法——`category: platform`（原 legacy `infra`）、`title: 服务编排` / `title_en: Supervisor`、`sort: 45`。网关侧的静态声明（使菜单条目永久化，按菜单盘点评审决定 D1）在 fusion-gateway#466 跟踪；legacy category 值无论如何都会在聚合时由 gateway#463 归一化。
- **服务进程组脱离（issue #60）：** 每个服务启动子进程（`exec_bash_script` + `StartSpec::Daemon` exec）都以 `process_group(0)` spawn——成为自己的进程组组长，而非继承守护进程的组。原因：重载 launchd 管理的守护进程（`launchctl bootout`）会向 job 的整个进程组发信号，过去会一枪 SIGTERM 整个受监督 fleet（`nohup` 只挡 SIGHUP，挡不住组信号）。脱离组后 daemon 重载只影响 daemon 自身；下次 boot 经探活优先跳过已健康服务重新收养仍在运行的服务，恢复是无操作而非大面积重启。停止路径不受影响（它们按 pidfile 而非进程组定位进程）。回归测试断言 exec 子进程落入不同 pgid。
- **非阻塞 stderr 捕获（issue #51）：** issue #47 的 stderr 尾部捕获不走管道——子进程 stderr 直接重定向进每服务日志文件，`wait()` 之后再读回尾部。以前的管道 + 读线程实现对那些 `start.sh` 会守护化后代的服务（uvicorn worker、后台服务器继承管道写端且永不关闭）会永久阻塞：`join()` 等管道 EOF，启动信号量许可泄漏，`up_all` 持核心互斥锁卡死，所有 RPC + `/api/v1/status` 挂起（只有无锁的 `/health` 能应答）。触发场景：`auto_up_on_start=true` 的全新主机开机。回归测试断言即使有持有 stderr 的残留孙进程，`exec_bash_script` 也及时返回。
- **守护化覆盖的按服务 `.env` 加载（issue #49）：** `StartSpec::Daemon` 增加 `env_file` 字段——nohup 之前，命令执行 `set -a; [ -f <file> ] || exit 1; source <file>; set +a`（路径相对 `FUSION_ROOT` 解析；文件缺失 = fail-closed 拒绝启动）。`fusion-k12-teacher` 用它 source 自己的 `.env`——项目的 key 材料（`FUSION_MLX_API_KEY` 等）在那里，而裸的 `fusion-k12 serve` 此前继承 daemon env 快照中的陈旧轮换 key：网关 401 → `/api/health` 503 `backend: down` → 每 5 分钟一轮探活重启循环。#49 同批：不可重启型探活失败（`BadPath`/`BadStatus`）的 `RestartFailed` 告警改为仅在转入 `Unhealthy` 时发出——持续不健康期间静默而非每 2s 一条；恢复后重新武装。
- **`.env.example` 中的密钥占位（issue #50）：** 网关 key 行是获取指引（`obtain-via-gateway-admin-api`），不是可用的假 key——旧的 `fg-admin-key` 字面量被直接拷进真实 `.env` 会产生 401 和一个"看起来正确"的配置。与 `architecture/config-conventions.md` §4 对齐。自配置 Phase 3（issue #74）起规范名是 `FUSION_CLIENT_KEY`（supervisor + compose 只消费这个名字；legacy `FUSION_MLX_API_KEY` 别名由 fusion-core 链路解析）。
- **历史：** SQLite（`~/.fusion-sv/history.db`，WAL 模式），`health_check` + `event` 表，7 天保留。保留清扫已接线（issue #37）：守护进程 tick 检查 `prune_schedule_sec`（默认每小时），从两张表清除超过 `history_retention_days`（默认 7 天）的旧行；每次清扫记录清除行数并累计进 `fusion_sv_events_pruned_total`。失败的清扫同样推进排期时间戳（无逐 tick 重试风暴——与备份调度同语义）。
- **告警持久化（issue #37）：** 每条通过去重的告警在 `Alerter::alert` 汇聚点持久化到 `event` 表（kind、detail、ts、plane、severity）——native 与 compose 告警一致（compose 服务去前缀存储并带 `plane="compose"`）。event 表带 `severity` 列（`error` = 容器 crash-loop，`info` = 恢复/drain/备份，`warn` = 其余）；旧库幂等迁移（`ALTER TABLE ... DEFAULT 'warn'`）。web 控制台的 Alerts 页和 `/api/v1/events` 由此回答"上周告过什么警"。`fusion_sv_alerts_total` 统计已发告警数。
- **告警：** `ServiceDown`（按服务 5 分钟去重）、`ServiceBack`、`RestartFailed`、`RestartCapHit` → 日志 + 可选 webhook。
- **登录延迟 tripwire（issue #12）：** 自动化 `~/actions-runner/RUNNERS.md` 的手工 tripwire（fusion#8 登录中断复盘：GitHub Actions self-hosted runner 与 fusion-mlx-server 共存时网关 `:11432` 认证延迟劣化到 16-18s）。守护进程派生的常驻循环（与核心锁分离，同备份任务模式）每 `tripwire_window_sec`（默认 30s）用 `pgrep -f tripwire_ci_process`（默认 `Runner.Listener`）检测 CI runner 活动；空闲窗口完全跳过探测（idle/active 切换各记一条 INFO）。活动窗口内测量 `tripwire_gateway_url`（默认 `http://127.0.0.1:11432/healthz`）与 `tripwire_identity_url`（默认 `http://127.0.0.1:11470/healthz`，未认证返回 401——响应时间即认证链前门延迟）的响应耗时，上限 `tripwire_timeout_ms`（默认 5000）；探测错误计为 breach。任一端点超过 `tripwire_threshold_ms`（默认 2000）即为 breach 样本；连续 `tripwire_consecutive`（默认 3）个 breach 样本经标准告警通道发一条 `LoginLatencyBreach` 告警（webhook `alert_url` + event 表，service `login-tripwire`，severity warn）——告警按 episode 闩锁：持续 breach 只告一次，直到出现干净样本才闭合 episode。每个探测样本写入 history.db `tripwire_sample` 表（`ts, gateway_ms, identity_ms, ci_active, breach`；NULL 延迟 = 探测错误）供复盘延迟曲线；保留清扫与其他历史表同步修剪。`fusion-sv tripwire` 可独立运行同一循环（无 daemon 锁、无监督 tick），用于演练与 launchd/cron 托管。见 `src/tripwire.rs`。
- **停止来源可追溯（issue #82）：** 每次 `down` 记录是谁发起的，与 supervisor 自行检测到的崩溃明确区分。CLI 发送自身 pid + argv（`origin: cli pid=123 argv: fusion-sv down`），HTTP 面打上 `http POST /api/v1/down from 127.0.0.1:xxxxx`，裸 UDS RPC 客户端标注为 origin-unknown。`down <svc>` 记一条 `manual stop (operator intent)` WARN 并写 `kind=ManualStop` 事件；全停（`down` 不带服务）额外在停机波前输出响亮的 5 行横幅并写 `kind=StopAll`（service `*`，severity warn）——事后取证直接从 event 表读来源，而不是靠时间戳关联推断（2026-10-30 22:53 事故）。`boot/group` 停机波的 RPC 日志行同样带来源。手工停机按设计保持停机（#79 时代语义）——无自动恢复；用 `fusion-sv up` 拉回 fleet。
- **Compose 平面（可选）：** `docker compose` 包在 `ComposeBackend` trait 之后。`up` = sidecar（`--wait`）→ native（按层排序）→ business（`--wait`）；`down` = compose `down --remove-orphans` → native 逆序。`status` 合并 native + 容器行（按 `plane` 标注）；`tick` 对 exited/unhealthy 容器告警（只监控，不崩溃重启——Docker restart policy 管这个）。对 `FUSION_PG_PASSWORD`/`FUSION_CLIENT_KEY` fail-closed。见 `src/compose.rs`。
- **NAS 备份（issue #10）：** `fusion-sv backup`（CLI 或 RPC）把 Postgres（`pg_dumpall` → gzip）和 Qdrant 快照（`storage` tar）转储到 `FUSION_BACKUP_TARGET`（`{target}/fusion-pg-{date}.sql.gz`、`{target}/fusion-qdrant-{date}/`）。由守护进程 tick 循环经 `backup_schedule_sec` 排期（默认每日）；保留 `backup_retention`（默认 7）份，清理更旧的集合。**Fail-closed：** `FUSION_BACKUP_TARGET` 未设 → 拒绝 + `BackupFailed` 告警（生产安全任务绝不静默跳过）。结果以 `kind=Backup` 记入 history.db。见 `src/backup.rs`。
- **Native 平面备份（issue #77）：** `compose.enabled = false`（默认）时备份 runner 指向权威的 native brew PostgreSQL（`127.0.0.1:5432`，按 `architecture/fusion-db-conventions-1030.md` §1），用本地 `pg_dumpall` 替代 `docker compose exec postgres pg_dumpall`——旧的仅 compose 路径曾静默跳过 dev 平面的真实数据库。二进制解析：`pg_dump_bin` 配置覆盖 → PATH → 裸名（spawn 失败在运行时 fail-closed 浮出）。有 `FUSION_PG_USER` 时作 `-U`（否则省略——brew trust 认证默认 OS 用户）；有 `FUSION_PG_PASSWORD` 时喂给 `PGPASSWORD`。native 平面的 Qdrant：runner 用 `docker ps` 探测 qdrant 容器，找到就快照；缺席（无 Docker、无容器）是 INFO 级事件跳过，不是错误。事件 detail 记录 `plane=native|compose`。
- **远程关停（issue #20）：** `fusion-sv shutdown`（CLI 或 RPC）经 UDS socket 发送一次性信号。守护进程主循环 select `ctrl-c` 或该信号，任一触发同一优雅路径（中止 tick 循环，返回）。幂等：第二次 `shutdown` 返回 `-32000 shutdown channel unavailable`（信号只能发一次）。`top` 是客户端侧命令（`fusion-sv top` 以 1s 清屏刷新循环 `fusion-sv status`）——其服务端 RPC 分支返回 `-32601` 并提示直接运行 `fusion-sv top`；无需服务端 RPC。
- **Prometheus /metrics（issue #22）：** 守护进程在 `metrics_addr`（默认 `127.0.0.1:9100`，仅 localhost——无认证，绑定即安全边界）提供 `/metrics` HTTP 端点（Prometheus text exposition format v0.0.4）。设 `metrics_addr = ""`（或缺省）禁用。手写 HTTP 服务器（`tokio::net::TcpListener` + `tokio::io`）——不引入 hyper/axum/prometheus crate。锁纪律：handler 锁 core → `metrics::render`（纯函数）→ 立即释放，与 RPC 同款短锁模式；绝不在 `.await` 间持有。指标：`fusion_sv_up 1`（守护进程存活）、`fusion_sv_services_total{plane}`（按 plane 计数）、`fusion_sv_service_healthy{service,port,plane}`（1/0）、`fusion_sv_service_state{service,port,plane,state}`（当前状态为 1）、`fusion_sv_crash_count{service}`（滑动窗口崩溃计数，仅 native），外加 issue #37 计数器 `fusion_sv_alerts_total`（去重后发出的告警）和 `fusion_sv_events_pruned_total`（保留清扫清除的历史行）。见 `src/metrics.rs`。
- **只读 HTTP status（issue #26）：** 同一 HTTP 监听器还提供 `GET /api/v1/status`——镜像 UDS `status` RPC 的 JSON 数组（`[{name, state, port, plane}]`，包装为 `{"ok": true, "services": [...]}`）。浏览器可达的 web 运维 dashboard 数据源；与 metrics 监听器一样只绑 loopback（绑定即安全边界）。设计上只读：start/stop/restart 留在带 token 的 UDS RPC 上；未知路径 404。见 `src/metrics.rs` 的 `metrics::render_status_json`。
- **Web 控制台（issue #25）：** HTTP 面在同一 `:9100` 监听器上长成完整控制台（`src/http_server.rs`）：`GET /api/v1/history?service=&limit=`（来自 SQLite 存储的按服务健康历史）、`GET /api/v1/events?limit=`（全局事件流——告警数据源）、`GET /api/v1/logs?service=`（日志尾部）、以及 `POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}`——变更操作原样复用 UDS `dispatch`，token 语义一致（`Authorization: Bearer` / `x-token` header 映射到 RPC `token` 参数）。配置 `web_dir` 后守护进程还从 `web/dist` 托管构建好的 SPA（`/` → `index.html`，拒绝路径穿越，仅文本资源）。控制台本体在 `web/`（React 18 + Vite + `@fusion/web-ui`，dev 端口 11473）：Services 页（状态表 + up/restart/drain/rollout，带危险操作确认对话框，spec S5）、Health 页（按服务延迟图）、Alerts 页（事件流）。见 `web/README.md`。
- **告警摄入（issue #139）：** `POST /api/v1/alerts/ingest` 接受外部告警生产者（如 fusion-ops VPS 健康探测经 `FUSION_OPS_ALERT_WEBHOOK`）。Body：`{"source":"fusion-ops","kind":"fail","severity":"error","detail":"...","dedup_key":"optional"}` —— `source`/`kind` 必填，`severity` 钳制到 `info|warn|error`（默认 `warn`），非字符串 `detail` 做 JSON 序列化。token 语义与其他变更操作一致（`Authorization: Bearer` / `x-token`）；token 无效或缺失 → 401 且不落事件。接受的告警以 `plane="external"` 落入 history.db 事件流（在 `GET /api/v1/events` 和 `fusion-sv top` 可见），并经配置的 `alert_url` webhook 重发（fire-and-forget，不重复落事件）。可选 `dedup_key`：`alert_dedup_sec`（默认 300s）内相同 key 返回 `{"ok":true,"deduped":true}` 不落事件；无 key = 每个 POST 都落。仅内存去重游标——守护进程重启即重开窗口。见 `src/ingest.rs`。

## Config

可选的 `~/.fusion-sv/config.toml`（所有字段都有默认值）。用 `FUSION_SV_CONFIG` 覆盖路径。配置路径解析（issue #136）：当 `FUSION_SV_CONFIG` 未设、默认路径不存在、而部署约定路径 `/opt/fusion-sv/config.toml` 存在时，CLI 采纳部署配置（与 systemd 守护进程读的同一文件）；两者并存时输出 WARN 点名两个路径，使静默分歧（一次性 CLI 与 daemon 之间 retention/schedule 不同）可归因。对以自定义配置启动的 daemon 跑一次性 CLI 时，务必显式设置 `FUSION_SV_CONFIG`：

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
# issue #145: systemd-managed services (VPS deploys where systemd owns the
# lifecycle). Declared services are started/stopped via `systemctl` instead of
# start.sh — start.sh pidfiles cannot control systemd-spawned processes, so a
# plain restart there fakes a stop and then aborts on port-occupied.
# Leave unset on hosts where start.sh owns the lifecycle (e.g. the Mac).
[systemd_units]
# "fusion-identity" = "fusion-identity.service"
# Prerequisite: the daemon user must be allowed to run `systemctl start/stop`
# on the declared units. If the daemon does not run as root, grant it via a
# polkit rule, e.g. /etc/polkit-1/rules.d/50-fusion-sv.rules:
#   polkit.addRule(function(action, subject) {
#       if (subject.user == "ops" &&
#           action.id == "org.freedesktop.systemd1.manage-units") {
#           var allowed = ["fusion-identity.service" /*, ... */];
#           if (allowed.indexOf(action.lookup("unit")) >= 0) {
#               return polkit.Result.YES;
#           }
#       }
#   });
# Without it, systemctl fails with "Interactive authentication required" and
# restart falls back to the port-occupied abort (fail-visible, not a fake stop).
metrics_addr = "127.0.0.1:9100"   # Prometheus /metrics (issue #22) + /api/v1/* face + web console (issue #25); "" = disable all
web_dir = "~/fusion/fusion-supervisor/web/dist"  # optional: serve the built web console from the same listener; "" = off
auto_up_on_start = true            # issue #29/#62: daemon boot runs up_boot_set once — T0 (fixed core) + pin=auto services (probe-skips already-healthy); host-reboot self-healing when the daemon itself is launched at boot. Default false. BEHAVIOR CHANGE in #62: was up_all (everything).

# Compose plane orchestration (disabled by default — native-only)
[compose]
enabled = false                    # set true to orchestrate container planes too
sidecar_file = "docker-compose.sidecar.yml"
business_file = "docker-compose.business.yml"
env_file = ".env"                  # fail-closed: needs FUSION_PG_PASSWORD + FUSION_CLIENT_KEY

# NAS backup (issue #10) — fail-closed if FUSION_BACKUP_TARGET unset
backup_schedule_sec = 86400        # daemon tick checks; 0 = first run always due. Default daily.
backup_retention = 7               # keep newest N pg+qdrant backup sets, prune older (issue #10)

# native-plane backup (issue #77) — used when compose.enabled = false
# pg_dump_bin = "/opt/homebrew/opt/postgresql@15/bin/pg_dumpall"  # default: resolve "pg_dumpall" from PATH
native_pg_host = "127.0.0.1"       # native brew PostgreSQL (authoritative dev instance)
native_pg_port = 5432

# daemon PATH augmentation (issue #80) — launchd PATH lacks Homebrew bin,
# node-based start.sh scripts fail under daemon launch. Appended (missing
# entries only) to the daemon's own PATH at start; children inherit. "" = off.
native_extra_path = "/opt/homebrew/bin:/usr/local/bin"

# history.db retention (issue #37) — sweep prunes health_check/event rows older than N days
history_retention_days = 7
prune_schedule_sec = 3600          # how often the sweep runs

# 登录延迟 tripwire（issue #12）— CI runner 活动窗口内的网关/身份延迟护栏
# （策略来源：~/actions-runner/RUNNERS.md，fusion#8 复盘）。样本写入 history.db tripwire_sample；
# 一个 breach episode 只发一条 LoginLatencyBreach 告警。
tripwire_enabled = true            # 守护进程常驻循环；false = 关闭
tripwire_window_sec = 30           # 探测节奏 + CI 活动复检节奏
tripwire_threshold_ms = 2000       # breach = 端点延迟严格大于该值
tripwire_consecutive = 3           # 每个告警 episode 所需的连续 breach 样本数
tripwire_timeout_ms = 5000         # 单次探测超时；探测错误计为 breach
tripwire_gateway_url = "http://127.0.0.1:11432/healthz"
tripwire_identity_url = "http://127.0.0.1:11470/healthz"   # 未认证 401 — 以响应时间为准
tripwire_ci_process = "Runner.Listener"                    # pgrep -f 模式；无匹配 = 空闲，不探测

# Scheduled jobs (issue #137) — one-shot binaries on a supervisor-driven schedule.
# Exactly one of daily_at (local HH:MM) / every_sec per entry; validation is
# fail-closed (bad job config refuses daemon start). Cursor persists in the
# meta table (job_last:<name>) so a daemon reload never re-fires a job.
# stdout/stderr append to ~/.fusion-sv/logs/job-<name>.log; env overlay from
# ~/.fusion-sv/env/<name>.env; outcome recorded as a `Job` event (success=info,
# failure=error + JobFailed escalation alert). Timeout kills at timeout_sec.
[[jobs]]
name = "fusion-ops-retention"
bin = "fusion-ops/fusion-ops-job"  # contains '/' → resolved relative to FUSION_ROOT
args = ["-job=retention"]
daily_at = "03:30"
timeout_sec = 900

[[jobs]]
name = "fusion-ops-cluster"
bin = "fusion-ops/fusion-ops-job"
args = ["-job=cluster"]
every_sec = 3600
```

（上面的 `fusion-ops` 条目仅为示例——自 issue #140 / fusion-ops#5 起，API 服务器及其 jobs 已迁到 VPS 上的 systemd 下运行；Mac 侧服务注册已移除，不再出现在 `fusion-sv status` 中。）

文件中每个字段都是可选的——只写想覆盖的键即可（自 v0.1.1 起全部有 serde 默认值；此前部分配置会以 `missing field socket_path` 失败）。

可选认证：在 daemon 侧设置 `FUSION_SV_TOKEN` 环境变量；CLI/客户端传 `params.token`。

## Shell 生态接管（issue #29）

四个 shell 模块服务以自定义启动命令 + 注册 env 受监督（2026-10-24 事故：主机重启静默杀掉它们，shell 侧边栏丢失全部产品入口）：

| Service | Port | Start command | Required env (injected, fail-closed) |
|---|---|---|---|
| fusion-cowork-web | 11461 | `fusion-cowork web serve --port 11461` (daemonized w/ pidfile) | `FUSION_SHELL_GATEWAY_URL`, `FUSION_ADMIN_KEY` |
| fusion-trainer-bridge | 8790 | `fusion-trainer/scripts/trainer-bridge.sh start` | `FUSION_TRAINER_BRIDGE_KEY` |
| fusion-plugins-ecosystem-console | 8795 | `fusion-plugin-console --port 8795` (daemonized w/ pidfile) | `FUSION_GATEWAY_BASE_URL`, `FUSION_ADMIN_KEY` |
| fusion-agent-studio | 11442 | `start.sh start` (default path) | — |

Env 解析：进程 env → `.env`（compose `env_file`）→ 兜底。`FUSION_ADMIN_KEY`（issue #54：按 env conventions Phase 2.2 从 `FUSION_GATEWAY_ADMIN_KEY` 改名）只读回退到 `fusion-gateway/config.yaml` 的 `web_shell.admin_key`（与 trainer-bridge.sh 同款模式）；`FUSION_GATEWAY_BASE_URL`（issue #54：从 `FUSION_SHELL_GATEWAY_URL` 改名）回退到 `http://127.0.0.1:11432`。`fusion-cowork` 的 shell registry 在上游过渡窗口内仍读旧 URL 名，因此 `fusion-cowork-web` 保留 `FUSION_SHELL_GATEWAY_URL` + `FUSION_ADMIN_KEY`；plugins console（v0.4.7）只认新名。`FUSION_TRAINER_BRIDGE_KEY` 无兜底——放进 `.env`（git 忽略）。Gateway 是 Core 层、模块是 App 层，所以 `up_all` 先拉起网关、模块再向它注册。

## Boot 分层 + 桌面托盘（issue #62）

实现 `architecture/fusion-supervisor-app-tiering-prd-1029.md`：服务划分 boot 层级，使主机重启只拉起真正重要的东西；外加一个 egui 悬浮面板做 fleet 控制。

**层级**（在 `architecture/port-registry.yaml` 中按服务标注为 `boot: {tier, pin}`——88 个条目）：

- **T0 core**（10 个）：mlx、gateway、memory、speech、identity、model-hub、rag、trainer、cowork、supervisor 邻接核心——开机必启，除非打 `manual` pin 否则不可跳过。
- **T1 platform**（约 20 个）：日常服务——仅当 pin 为 `auto` 时开机启动（按用户覆盖，存于 `~/.fusion-sv/history.db` 的 `boot_pin` 表），否则按需。
- **T2 app/domain**（其余）：从不在开机启动；按需启动（托盘 / web 控制台 / CLI）。

Boot 集合 = T0 ∪ {pinned=auto}。`~/.fusion-sv/config.toml` 中 `auto_up_on_start = true` 现在执行 **`up_boot_set`**（T0 + pins）而非 `up_all`——**行为变更**：过去 daemon 开机会把所有受管服务全部拉起；现在主机重启恢复只还原核心 + 你 pin 过的平台服务，削减开机内存占用与启动扰动。不带参数的 `fusion-sv up` 仍意味着全部服务。

HTTP 面新增（与其他变更操作同一 token 规则）：

- `GET /api/v1/boot` — 每服务一行：`{name, tier, pinned, state, port}`（单一端点；托盘只轮询它——携带 tier+pin+state，是 `/api/v1/status` 的超集）。
- `POST /api/v1/boot/pin` `{"service": "...", "mode": "auto"|"manual"|"clear"}` — 按用户 boot 覆盖；daemon 重启后仍在（SQLite）。
- `POST /api/v1/boot/group` `{"group": "T0"|"T1"|"T2", "action": "up"|"down"}` — 层级批量操作。

### 桌面托盘（`fusion-tray`）

```bash
cargo build --release --workspace   # binary: target/release/fusion-tray
./target/release/fusion-tray        # no config needed; env overrides below
```

一个停靠在屏幕右缘（垂直居中）的 14px 圆点：绿 = 全部 T0 健康，黄 = T0 降级，红 = T0 宕机，灰 = daemon 不可达。**圆点可拖到任意位置**（按住拖动；窗口逐帧跟随指针）——松手时距屏缘 20px 内吸附到最近屏幕边缘，位置持久化到 `~/.fusion-sv/tray.json` 并在下次启动恢复（越界的保存位置会被夹回屏内，例如分辨率变化后；文件损坏回退默认位置）。点击 → 扇形菜单，**朝远离最近屏幕边缘的方向展开**（圆点在右缘向左展开，左缘向右，上缘向下，下缘向上；最近边缘优先，并列取水平）。半圆始终按展开方向的前导侧排三个层级扇区：向左展开时顶 = CORE、中 = PLATFORM、底 = APP（其余三个方向镜像）——3 个层级扇区带每层聚合色和每层服务圆点 → 点击扇区 → 服务列表（放在圆点有空间的一侧，夹在屏内；运行中优先排序、实时搜索、每服务 start/stop/pin、右键 restart/drain/rollout；CORE 的 stop 需确认）。每次窗口几何变化记一行 `窗口几何已应用` 日志（state、选定方向、圆点与窗口位置），因此展开方向可从 `~/.fusion-sv/logs/tray.log` 排障而无需截图。**每个层级都有一键拉起和一键停止**（`POST /api/v1/boot/group`；组停止是破坏性的，先确认）。ESC 逐级回退：列表 → 扇形 → 圆点。破坏性操作（restart/drain/rollout/组停止/全停）先确认。

daemon 不可达时圆点变灰，面板显示 "daemon 未运行" 提示并带一键自愈（`fusion-sv up`）；不白屏。不可达由两步探测判定：先 `GET /api/v1/boot`，再 `GET /health`（无锁 handler）作为"忙 vs 死"判别器——`/boot` 超时而 `/health` 存活时保持当前视图并记 "daemon 忙"，而非闪烁不可达提示（批量组操作会持核心互斥锁数秒）。Env：`FUSION_SV_BASE`（默认 `http://127.0.0.1:9100`）、`FUSION_SV_TOKEN`、`FUSION_TRAY_THEME`（`dark`/`light`，默认跟随系统外观）。轮询节奏：面板打开 5s / 圆点 30s / 不可达 60s。

**常驻进程鲁棒性 + 内存**（PRD 审计加固）：

- **CJK 字体子集**：托盘内嵌 `tray/assets/cjk-subset.ttf`（约 132KB，`include_bytes!` + `FontData::from_static`，零拷贝）而非运行时读取 22.4MB 的系统 Hiragino Sans GB——全量字体过去以 3 份 malloc 拷贝（约 70MB）加字形缓存常驻，把 RSS 推到约 180MB。子集化后：RSS 约 100MB（含 GPU/GL 基线），`MALLOC_LARGE` 69MB → 2MB。新增中文文案后运行 `bash tray/gen_font_subset.sh && cargo test -p fusion-tray`；`font_tests::test_subset_font_covers_source_charset`（ttf-parser）在子集与源字符集漂移时使构建失败。
- **panic 隔离**：poller 和 worker 线程把每轮包在 `catch_unwind` 里（panic 记 ERROR 并继续，而不是杀死托盘）；daemon tick 循环把每轮 spawn 成内部 task，使 `tick_all` 的 panic 无法在 RPC/HTTP 存活时静默停止监督。托盘所有互斥锁用毒锁恢复（`.lock().unwrap_or_else(|p| p.into_inner())`），panicked 线程不会级联。
- **手工测试清单**（`tray/TESTING.md`，issue #76）：单元测试覆盖不到的视觉/交互层（圆点颜色映射、呼吸、拖动+吸附、扇形方向、乐观切换、pin 语义、确认流、daemon 宕机自愈、单实例守卫、LaunchAgent 登录）。任何 winit/egui 升级或几何重构后重跑相关部分。
- **透明圆点窗口**：L1 圆点窗口覆盖 `eframe::App::clear_color` 为全透明（eframe 默认 `rgba(12,12,12,180)` 会在半圆点后面画灰色方块底）；fan/list/unreachable 状态保留各自不透明的面板背景。
- **单实例**：`main()` 在启动 GUI 前获取一个纯 std 的 UDS 锁 socket（`~/.fusion-sv/tray.lock`）。二次启动探测到存活的首实例（connect 探测）即退出并记一行日志；崩溃残留的陈旧 socket 文件被移除后重绑一次。锁完全拿不到时进程拒绝启动——绝不出两个悬浮点。注意停止边界：组停止只能到达 supervisor 自己启动的服务——由各自 LaunchAgent 拉起的进程（KeepAlive 反正会重启它们）或手工 `nohup` 的进程不在其覆盖范围内。launchd 管理的服务从不写 `start.sh` 要读的 pidfile，所以 `start.sh stop` 打印 "not running" 并 exit 0：supervisor 认为已停止，下一次探活又把它翻回 Healthy。自 2026-10-30 起，`fusion-comfyui`（原 `com.fusion-comfyui.server`）和 `fusion-gupiao`（原 `com.fusion.gupiao`）已从 launchd 迁到 supervisor 托管——plist 移入 `~/.fusion-sv/launchagents-disabled-20261030/`，服务改经 `bash <repo>/start.sh start` 启动（写 `.run/comfyui.pid` / `~/.fusion-gupiao/run.pid`），因此 `fusion-sv down <svc>`、托盘和组停止都能真正停掉它们。gupiao 的 scheduler 由其自身配置默认值自启（`FUSION_GUPIAO_SCHEDULER_AUTOSTART=1`）。已知缺口（v0.1.28 已关闭）：迁移丢失了旧 plist 的 `caffeinate -i` 防 idle-sleep 守护。supervisor 现以过渡陪护方式恢复它：`fusion-gupiao` 的 `start`/`restart` 成功后 spawn 一个 detached 的 `caffeinate -i -w <服务 pid>`（`start_map::caffeinate_pidfile` 表），在服务生命周期内持有 `PreventUserIdleSystemSleep` 断言，服务退出或被停时自动释放——01:00 的 `daily_suggest` 长扫描再次安全，无需手工命令。上游正解（gupiao `start.sh` 的 caffeinate 选项）在 fusion-gupiao#6 跟踪；落地后可删 supervisor 陪护表项。恢复 launchd 托管：先 `fusion-sv down <svc>`，再把 plist 移回并 `launchctl bootstrap gui/$UID <plist>`。

### LaunchAgent（登录即 daemon + 托盘常驻）

两个 agent 带来完整的重启自愈：daemon 以生产 env 启动，`auto_up_on_start = true` 拉起 boot 集合，然后托盘连上它。

```bash
# daemon agent (restored from ~/.fusion-sv/launchagents-backup-20261029/, carries prod env secrets, 600 perms)
cp ~/.fusion-sv/launchagents-backup-20261029/com.fusion.supervisor.plist ~/Library/LaunchAgents/
chmod 600 ~/Library/LaunchAgents/com.fusion.supervisor.plist
# tray agent (no secrets; generate from template)
sed "s|__FUSION_SUPERVISOR_DIR__|$(pwd)|" scripts/com.fusion.tray.plist.template \
    > ~/Library/LaunchAgents/com.fusion.tray.plist
launchctl load ~/Library/LaunchAgents/com.fusion.supervisor.plist
launchctl load ~/Library/LaunchAgents/com.fusion.tray.plist
```

## 与既有监督者的边界

- **fusion-gateway** 仍是推理路由入口。其 `auto_start` 只启动 fusion-mlx；supervisor 管理所有受管服务（包括 mlx）。避免双重启动：supervisor 先探活，已健康则跳过。（边界在 fusion-gateway issue 跟踪。）
- **fusion-studio/Scripts/start.sh** → legacy。supervisor 使用 registry 真源，不继承脚本的陈旧值。（Studio 迁移在 fusion-studio issue 跟踪。）
- **fusion-cli** 的 `service`/`rag`/`doc` start/stop 保留用于交互便利；supervisor 是权威。（可选的 `fusion net` 薄子命令在 fusion-cli issue 跟踪，v1.1。）

## Test

```bash
cargo test --workspace --all-targets   # 227 test fns (unit + 13 integration files + tray)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Acceptance (v1)

- [x] `cargo build --release` → `fusion-sv`, 0 warnings (clippy -D warnings)
- [x] `cargo test` layer 1+2 all pass (49 tests)
- [ ] `fusion-sv daemon` then `fusion-sv status` lists all managed services (real registry)
- [ ] `fusion-sv up` starts core (mlx+gateway), skips already-running (no double-start)
- [ ] Manually kill a non-core service → recovers within 30s, history recorded
- [ ] Kill same service 5x → `Failed` state, stops restart, `RestartCapHit` alert
- [ ] `fusion-sv down` reverse-order stop, confirms `Stopped`
- [ ] Probe path error (404) → no restart (anti-storm), alert only
- [ ] `fusion-sv top` readable live table, per-service log files
- [x] Process data cleaned after testing, only final binary + logs kept
- [x] README generated (this file)

## Non-goals (v1)

跨节点 Prometheus 聚合（v2）、跨节点集群编排、改写 start.sh、launchd 自守护（v1.1）、配置热重载、多用户。（Web 控制台还是提前交付了——见 `web/`。）

## 容器多节点部署

容器化多节点商业部署拓扑——扩展 `architecture/fusion-deploy-container-multinode-0901.md`。分层 Docker Compose：base 集群平面（`fusion-multi-nodes/docker-compose.yml`，network pin #58 已落地）+ 业务平面 overlay（本仓的 `docker-compose.business.yml`）。合并运行：`docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d`。

**容器化的部分：** 集群平面（Master + Agent）+ HTTP 业务平面（现为 model-hub、cowork；gateway/rag/artifacts/code/doc 等上游 Dockerfile PR 落地后跟进）。**保持裸金属的部分：** MLX（`localhost:11434`，Metal / 统一内存）、UDS 守护平面（memory/speech——UDS 不跨容器边界）。

### Prerequisites

1. MLX 以裸金属运行：`~/claude-home/fusion-mlx/start.sh start`
2. 集群 token + MLX API key 行内导出（base compose 是 fail-closed 的 `${FUSION_CLUSTER_TOKEN:?}` / `${FUSION_MLX_API_KEY:?}`——`fusion-multi-nodes` 中不创建 `.env`）：
   ```bash
   export FUSION_CLUSTER_TOKEN=$(openssl rand -hex 16)
   export FUSION_MLX_API_KEY=fg-admin-key   # matches fusion-mlx api_key
   ```
3. 冒烟模型已缓存：`mlx-community-Llama-3.2-1B-Instruct-4bit`（缺失时经 `HF_MIRROR=https://hf-mirror.com` 下载），在 MLX 中可用。
4. Docker Desktop 运行中（v29.4+）。
5. 先停掉裸金属的 `fusion-multi-node` master（它占 11452 端口）：
   `cd fusion-multi-nodes `cd fusion-multi-node && bash start.sh stop``cd fusion-multi-node && bash start.sh stop` bash start.sh stop`。

### Build the smoke images

overlay 的 Dockerfile COPY 的是 monorepo 根相对路径（requirements.lock、`fusion-core/`、`fusion-cowork/`），因此构建上下文必须是 monorepo 根——但根目录 200G+ 且没有根 `.dockerignore`，根上下文构建不可行。harness 改为先布置一个精简构建上下文：

```bash
bash scripts/prepare-build-ctx.sh   # idempotent: populates .build-ctx/ (git-ignored)
```

然后 harness 构建 `fusion-cowork:smoke`（真实上游 Dockerfile）和 `fusion-model-hub:smoke`（scoped 包装 `scripts/Dockerfile.model-hub-smoke`，安装 model-hub 自己的 pyproject 依赖而非整个 monorepo lock——见上游 issue dahai80/fusion-models-hub#52）。

### Run smoke validation

多节点冒烟 harness（`scripts/smoke-container-multinode.sh`）已在 PR #24 **移除**——它依赖 `fusion-multi-nodes/docker-compose.yml`，而后者已不在本地单机部署上测试。sidecar 平面冒烟（`bash scripts/smoke-sidecar.sh` + 纯单元的 `bash scripts/smoke-sidecar.test.sh`）仍是经过验证的路径。

### 无冒烟直接拉起拓扑

```bash
cd fusion-multi-node
docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d
```

拆除：`docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml down --remove-orphans`

### Cleanup（过程数据）

冒烟 harness 经 `trap` 自动拆除——不留持久测试数据，仅保留 `/tmp/fusion-smoke.*/smoke.log` 运行日志（按 CLAUDE.md：只留最终输出 + 日志）。`fusion-cluster` Docker 网络可能留存（无害，跨次运行复用）。手工拆除 + 证书清理（mTLS 路径）：`docker compose ... down` + `rm -rf fusion-multi-nodes/tls/`。

## Production Sidecar (Traefik + Redis + Postgres + Qdrant)

实现 `architecture/fusion-deploy-0902.md` §1.1——面向 10 人 Phase 2 生产部署的 OrbStack sidecar 中间件/数据层。native 核心（MLX + fusion-gateway，Metal/VRAM）留在主机上；sidecar 在容器中运行反向代理、队列/锁/限流存储、关系元数据与向量库。

| Service | Image | Host port | Role |
| --- | --- | --- | --- |
| traefik | traefik:v3.1 | 80/443/8080 | TLS, domain routing, CORS, routes to native gateway + containerized apps |
| redis | redis:7-alpine | 6379 | quota/lock/rate-limit tokens (gateway `budget.go`, identity concurrency) |
| postgres | postgres:16-alpine | 5432 | shared relational DB (cowork/identity/rag/modelhub — one instance, per-app DBs) |
| qdrant | qdrant/qdrant:v1.12.4 | 6333/6334 | fusion-rag vector DB, tenant-prefix isolation |

端口遵守 `architecture/port-registry.yaml`——sidecar 中间件使用保留的标准端口，与 fusion 服务端口（11xxx）无冲突。

### 一键拉起

```bash
cd fusion-supervisor
cp .env.example .env          # fill FUSION_PG_PASSWORD + FUSION_CLIENT_KEY (required)
bash scripts/prepare-build-ctx.sh   # stage lean build context for business images
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml up -d
```

这会拉起 traefik + redis + postgres + qdrant + model-hub + cowork，全部 healthy。业务 overlay（`docker-compose.business.yml`）依赖 sidecar 服务（redis/postgres healthy），经共享 `DATABASE_URL`/`REDIS_URL` 连接——每个应用不再自建 Postgres（收敛掉仅 dev 的 cowork postgres）。native 的 MLX/gateway 经 `host.docker.internal:11434` / `:11432` 接入。

### Postgres 多库初始化

单一 PG 实例在首次启动时为每个应用建一个库（`POSTGRES_MULTIPLE_DATABASES=cowork,identity,rag,modelhub`），经 `scripts/pg-init-multiple-db.sh`。每个应用按库名连接；RLS / 按租户隔离仍是各应用自己的责任。

### Traefik 路由

每个业务服务上的 Docker labels 注册 Traefik 路由器（`websecure` entrypoint 上启用 TLS）。静态动态配置（`traefik/dynamic.yml`）把 `/v1` + `/api/gateway` 路由到 **native** 的 fusion-gateway（`host.docker.internal:11432`）。API-key → 租户解析仍留在 fusion-identity（上游）——Traefik 只转发 key；gateway 打上 `X-Fusion-Tenant` 并拒绝无效 key（认证真源不在 compose 中重复）。

### Sidecar 冒烟验证

```bash
bash scripts/smoke-sidecar.test.sh      # unit (no Docker): compose presence + guards
bash scripts/smoke-sidecar.sh           # live: 5-step sidecar bring-up + overlay merge
```

活体冒烟（设计 §1.1）：precheck → sidecar 拉起 → PG 多库检查 → 业务 overlay 上共享 PG/Redis → Traefik 路由表。通过横幅 = `SMOKE PASS`。经 `trap` 自动拆除。日志：`/tmp/fusion-smoke-sidecar.*/smoke.log`。

### Teardown

```bash
docker compose -f docker-compose.sidecar.yml -f docker-compose.business.yml down --remove-orphans
```

卷（`pg_data`、`qdrant_data`、`redis_data`、`traefik_letsencrypt`）在 `down` 后留存，除非加 `-v`。PG/Qdrant 每晚备份到共享 NAS 是运维任务（§5.4）——`FUSION_BACKUP_TARGET` 见 `.env.example`。

