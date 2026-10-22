# Fusion Supervisor Web Console

Web console for `fusion-sv` (issue #25). Three pages over the supervisor HTTP
face on `127.0.0.1:9100`:

- **Services** — live status table (5s poll) with up / restart / drain /
  rollout per service and up-all / down-all. Destructive operations require a
  confirmation dialog (danger styling, spec S5).
- **Health** — per-service latency line chart + sample/last-status counters
  from `/api/v1/history` (10s poll).
- **Alerts** — global event stream from `/api/v1/events` (crash, restart,
  drain, rollout, backup events) with relative timestamps (10s poll).

Read endpoints (`/api/v1/status`, `/api/v1/history`, `/api/v1/events`) need no
token. Mutations (`POST /api/v1/{up,down,restart,drain,rollout}`) reuse the
UDS RPC dispatch, so they require the supervisor token (`FUSION_SV_TOKEN`)
sent as `x-token` (or `Authorization: Bearer`). The web app reads it from
`VITE_SV_TOKEN` at build time — for dev convenience only; in embedded mode
token injection belongs to the shell host.

## Stack

React 18 + Vite 5 + TypeScript + Tailwind (fusion preset) + Bun, sharing
`@fusion/web-ui` components (StatusBadge, MetricChart, ConfirmDialog,
DataTable) and `@fusion/web-sdk` via `file:vendor/@fusion` — zero registry
dependencies.

## Dev

```bash
cd web
bun install
bun run dev        # vite on 11473, proxies /api /metrics /health → 9100
bun run build      # typecheck + vite build → dist/
bun run lint       # biome
```

`FUSION_DEV_BACKEND` overrides the backend target (default
`http://127.0.0.1:9100`). `FUSION_SV_WEB_PORT` overrides the dev port.

## Serving from the supervisor daemon

The daemon serves the built SPA from the same `:9100` listener when
`web_dir` is configured in `~/.fusion-sv/config.toml`:

```toml
web_dir = "/Users/<user>/fusion/fusion-supervisor/web/dist"
```

Then `http://127.0.0.1:9100/` serves the console (hash routes, SPA fallback
to `index.html` semantics via relative asset base).

## Gateway wiring (embedded mode)

The `/shell/fusion-supervisor/` proxy is static gateway configuration:

```toml
[web_shell.modules.fusion-supervisor]
upstream = "http://127.0.0.1:9100"
```

The shell registry manifest lives in `web/manifest.json` (category `infra`,
devices desktop + mobile-ready, `api_key_env` = `FUSION_SV_TOKEN`).
