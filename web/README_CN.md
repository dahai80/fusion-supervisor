# Fusion Supervisor Web 控制台

`fusion-sv` 的 Web 控制台 (issue #25)。三个页面, 数据来自 supervisor 的 HTTP 面
`127.0.0.1:9100`:

- **Services** — 实时状态表 (5s 轮询), 支持单服务 up / restart / drain /
  rollout 与全量 up-all / down-all。危险操作需确认弹窗 (danger 样式, spec S5)。
- **Health** — 按服务的延迟折线图 + 采样数/最近状态统计, 数据来自
  `/api/v1/history` (10s 轮询)。
- **Alerts** — 全局事件流 `/api/v1/events` (crash/restart/drain/rollout/
  backup 事件), 相对时间戳 (10s 轮询)。

只读端点 (`/api/v1/status`, `/api/v1/history`, `/api/v1/events`) 无需 token。
变更操作 (`POST /api/v1/{up,down,restart,drain,rollout}`) 复用 UDS RPC
dispatch, 需要 supervisor token (`FUSION_SV_TOKEN`), 通过 `x-token` (或
`Authorization: Bearer`) 传递。Web 构建时从 `VITE_SV_TOKEN` 读取 — 仅 dev
便利; 嵌入模式的 token 注入属于 shell 宿主。

## 技术栈

React 18 + Vite 5 + TypeScript + Tailwind (fusion preset) + Bun, 复用
`@fusion/web-ui` 组件 (StatusBadge/MetricChart/ConfirmDialog/DataTable) 与
`@fusion/web-sdk`, 通过 `file:vendor/@fusion` 引入 — 零 registry 依赖。

## 开发

```bash
cd web
bun install
bun run dev        # vite 11473, 代理 /api /metrics /health → 9100
bun run build      # typecheck + vite build → dist/
bun run lint       # biome
```

`FUSION_DEV_BACKEND` 覆盖后端地址 (默认 `http://127.0.0.1:9100`)。
`FUSION_SV_WEB_PORT` 覆盖 dev 端口。

## 由 supervisor daemon 直接服务

`~/.fusion-sv/config.toml` 配置 `web_dir` 后, daemon 在同一 `:9100` listener
上服务构建产物 SPA:

```toml
web_dir = "/Users/<user>/fusion/fusion-supervisor/web/dist"
```

访问 `http://127.0.0.1:9100/` 即控制台 (hash 路由, 相对资源 base)。

## 网关接线 (嵌入模式)

`/shell/fusion-supervisor/` 代理是网关静态配置:

```toml
[web_shell.modules.fusion-supervisor]
upstream = "http://127.0.0.1:9100"
```

shell 注册 manifest 见 `web/manifest.json` (category `infra`, devices
desktop + mobile-ready, `api_key_env` = `FUSION_SV_TOKEN`)。
