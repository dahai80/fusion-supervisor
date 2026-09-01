# Container Multi-Node Deploy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land a layered Docker Compose overlay + smoke-validation harness that proves the containerized multi-node commercial-deployment topology on a single M5 128G, routing one inference through Master→Agent→MLX with auth handshake.

**Architecture:** Base `docker-compose.yml` (cluster plane — fusion-multi-node, already landed incl. network pin #59) + new overlay `docker-compose.business.yml` (business plane — lives in fusion-supervisor per governance). Merge via `docker compose -f <base> -f <overlay> up`. MLX + UDS guardian plane stay bare-metal (Metal/UDS don't cross container boundary). Containers reach MLX via `host.docker.internal:11434`. A `scripts/smoke-container-multinode.sh` harness validates the 5 smoke steps from design §5.1.

**Tech Stack:** Docker Compose v2 (daemon v29.4), bash smoke harness, curl, fusion-multi-node v0.14.0 (Master :11452 + Agent :11458), fusion-mlx (bare-metal :11434), Python/FastAPI + Go + Bun business services.

**Spec:** `architecture/fusion-deploy-container-multinode-0901.md` (monorepo root) — the design this plan argues from. Executor reads both.

## Global Constraints

- **Governance:** Only modify `fusion-supervisor/`. Upstream gaps (5 missing Dockerfiles) resolved via issue→PR→land (6 issues filed 2026-09-01; #58 landed as #59). Do NOT edit fusion-multi-node / fusion-gateway / fusion-rag / etc. directly.
- **MLX stays bare-metal:** `~/claude-home/fusion-mlx/start.sh start|stop`. Real model load for inference test (CLAUDE.md: 涉及大模型测试须真实加载模型). Download via `HF_MIRROR=https://hf-mirror.com`.
- **No port moves:** `architecture/port-registry.yaml` (41 services) authoritative. Containers host-map same ports; fusion-sv probes unchanged. Design does NOT touch supervisor probe code.
- **UDS boundary:** fusion-memory (`/tmp/fusion-memory.sock`) + fusion-speech stay bare-metal — UDS doesn't cross container boundary. rag→memory needs HTTP shim or rag stays bare-metal (design §2.3).
- **Security 3-layer:** cluster token (always-on, fail-closed `:?`); mTLS (opt-in, fail-closed); RBAC (documented, not smoke-exercised). Smoke uses mTLS-off + token-only (`--scale agent=N` breaks mTLS leaf certs — smoke uses default 1 agent or 2 individually-provisioned).
- **Resources (lean):** business containers 2GB/2cpu; Master 4GB/4cpu; Agent 4GB/4cpu. 2-3 agents + 7 business ≈ 24-32GB, leaves ~96GB for MLX + host.
- **Cleanup:** After validation, `docker compose down` (both files), delete `./tls/` test certs. Keep only smoke logs + design doc. No persistent test data (CLAUDE.md).
- **Code style:** 4-space multiples indentation, no docstrings, always include logging, clean code no wrappers.
- **Smoke depth:** "it works" level — topology up + 1 routed inference + auth handshake. NOT: load/rate-limit/KV-sharing stress/HA/flows A-D (design §5.3, Phase 2).

## Upstream PR Dependency Status (user-tracked)

| # | Repo | Change | Issue | Status |
|---|------|--------|-------|--------|
| 6 | fusion-multi-node | base compose network `name: fusion-cluster` pin | [#58](https://github.com/dahai80/fusion-multi-node/issues/58) | ✅ LANDED (PR #59, commit 70c50a7) |
| 1 | fusion-gateway | Dockerfile (Go multi-stage) | [#143](https://github.com/dahai80/fusion-gateway/issues/143) | ⏳ pending |
| 2 | fusion-rag | Dockerfile (py slim) | [#55](https://github.com/dahai80/fusion-rag/issues/55) | ⏳ pending |
| 3 | fusion-artifacts-engine | Dockerfile (py slim) | [#53](https://github.com/dahai80/fusion-artifacts-engine/issues/53) | ⏳ pending |
| 4 | fusion-code | Dockerfile (Bun compile) | [#215](https://github.com/dahai80/fusion-code/issues/215) | ⏳ pending |
| 5 | fusion-doc | Dockerfile (node slim) | [#41](https://github.com/dahai80/fusion-doc/issues/41) | ⏳ pending |

**Tasks 1-6 execute NOW** (use only model-hub + cowork, whose Dockerfiles already exist). **Task 7 is GATED** — add the remaining 5 services once their PRs land.

## File Structure

- **Create:** `fusion-supervisor/docker-compose.business.yml` — overlay declaring `fusion-cluster` external + business-plane services. Lives at supervisor repo root so `build: ../fusion-gateway` resolves to `/Users/dahai/fusion/fusion-gateway`. Initially 2 services (model-hub + cowork — Dockerfiles exist); Task 7 adds 5 more after upstream lands.
- **Create:** `fusion-supervisor/scripts/smoke-container-multinode.sh` — bash harness running design §5.1's 5 smoke steps (precheck → cluster up → business up → routed inference → auth proof → teardown). Idempotent; logs to `logs/smoke-container-multinode-<ts>.log`.
- **Modify:** `fusion-supervisor/README.md` — add "Container Multi-Node Deploy" section (how to run overlay + smoke).
- **Create:** `fusion-supervisor/scripts/smoke-container-multinode.test.sh` — lightweight unit checks for the harness (env precheck logic, compose-file presence, token-nonempty guard) runnable without Docker, so the harness has a real test cycle per writing-plans.

### File responsibilities

- `docker-compose.business.yml` — pure declarative topology. No logic. Each service: build path, host-map port, MLX env, `extra_hosts`, network, restart, logging, mem/cpu limits. Validated by `docker compose config`.
- `scripts/smoke-container-multinode.sh` — imperative validation. Reads env (TOKEN, MLX_API_KEY), runs compose up/down, curls health + task submit, asserts status codes, logs every step, exits non-zero on any failure, tears down in `trap`.
- `scripts/smoke-container-multinode.test.sh` — tests the harness's pure functions (precheck, path resolution) without Docker. Run by `bash scripts/smoke-container-multinode.test.sh`.

---

## Task 1: Overlay compose — model-hub + cowork (Dockerfiles exist)

**Files:**
- Create: `fusion-supervisor/docker-compose.business.yml`
- Test: `fusion-supervisor/scripts/test-overlay-compose.sh` (validate overlay reconciles with base via `docker compose config`)

**Interfaces:**
- Consumes: base `fusion-multi-node/docker-compose.yml` network `fusion-cluster` (pinned `name: fusion-cluster`, landed PR #59). model-hub health at `:11444/api/v1/system/health` (confirmed from `fusion-model-hub/Dockerfile` HEALTHCHECK). cowork health at `:11438/health` (confirmed from `fusion-cowork/Dockerfile` HEALTHCHECK).
- Produces: overlay file referenced by Tasks 2-7 as `docker compose -f ../fusion-multi-node/docker-compose.yml -f docker-compose.business.yml ...`. Service names `fusion-model-hub`, `fusion-cowork` for later `docker compose ... ps` filtering.

- [ ] **Step 1: Write the failing test (compose-config validation)**

Create `scripts/test-overlay-compose.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
# validate overlay reconciles with base compose — no "network not found", no syntax error.
cd "$(dirname "$0")/.."
BASE=../fusion-multi-node/docker-compose.yml
OVERLAY=docker-compose.business.yml
test -f "$BASE" || { echo "FAIL: base compose missing at $BASE"; exit 1; }
test -f "$OVERLAY" || { echo "FAIL: overlay missing at $OVERLAY"; exit 1; }
# base compose uses ${VAR:?...} fail-closed — config interpolation needs vars present (any value).
export FUSION_CLUSTER_TOKEN="${FUSION_CLUSTER_TOKEN:-test-token-not-real}"
export FUSION_MLX_API_KEY="${FUSION_MLX_API_KEY:-test-key-not-real}"
# config merges both files; exit 0 = reconciles, external network resolves.
# capture rc without set -e killing the script (|| RC=$? pattern).
RC=0
docker compose -f "$BASE" -f "$OVERLAY" config >/dev/null 2>&1 || RC=$?
if [ $RC -ne 0 ]; then
    echo "FAIL: docker compose config rejected the merge (exit $RC)"
    docker compose -f "$BASE" -f "$OVERLAY" config 2>&1 | tail -5
    exit 1
fi
# overlay must declare fusion-cluster external — proves it joins base's network, not creates its own.
docker compose -f "$BASE" -f "$OVERLAY" config 2>&1 | grep -q 'name: fusion-cluster' \
    || { echo "FAIL: fusion-cluster name-pin not present in merged config"; exit 1; }
echo "PASS: overlay reconciles with base, fusion-cluster pinned"
```

`chmod +x scripts/test-overlay-compose.sh`.

- [ ] **Step 2: Run test to verify it fails**

Run: `bash scripts/test-overlay-compose.sh`
Expected: FAIL with "overlay missing at docker-compose.business.yml" (file not created yet).

- [ ] **Step 3: Write minimal implementation — the overlay**

Create `docker-compose.business.yml` at supervisor repo root:

```yaml
# fusion-supervisor 业务面 overlay — 与 fusion-multi-node/docker-compose.yml (base, 集群面) 叠加。
# 单独不可用 (依赖 fusion-cluster 网络, 由 base 创建); 合并启动:
#   docker compose -f ../fusion-multi-node/docker-compose.yml -f docker-compose.business.yml up -d
# 每服务 host-map 注册端口 (port-registry.yaml 权威), fusion-sv 探活不变。
# MLX 裸机: 经 host.docker.internal:11434 回连 (Docker Desktop 内置; Linux 由 base extra_hosts 解析)。
# UDS 守护面 (memory/speech) 不容器化 — UDS 不过容器边界, 留裸机 (见设计 §2.3)。
networks:
  fusion-cluster:
    external: true
    name: fusion-cluster

services:
  fusion-model-hub:          # Dockerfile 已有 — health :11444/api/v1/system/health
    build: ../fusion-model-hub
    ports: ["11444:11444"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_MLX_API_KEY: "${FUSION_MLX_API_KEY:?必须设置 FUSION_MLX_API_KEY — fusion-mlx 启用的 api_key}"
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"

  fusion-cowork:             # Dockerfile 已有 — health :11438/health
    build: ../fusion-cowork
    ports: ["11438:11438"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_MLX_API_KEY: "${FUSION_MLX_API_KEY:?必须设置 FUSION_MLX_API_KEY}"
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `bash scripts/test-overlay-compose.sh`
Expected: PASS — "overlay reconciles with base, fusion-cluster pinned".
(Requires Docker daemon running — confirmed v29.4. If daemon down, start Docker Desktop first.)

- [ ] **Step 5: Commit**

```bash
git add docker-compose.business.yml scripts/test-overlay-compose.sh
git commit -m "feat(deploy): add business-plane compose overlay (model-hub + cowork)

Layered compose: base docker-compose.yml (cluster plane, fusion-multi-node
#59 network pin landed) + new docker-compose.business.yml overlay. Overlay
declares fusion-cluster external:true, joins base's pinned network. 2 services
initially (model-hub + cowork — Dockerfiles exist); 5 more gated on upstream
PRs (gateway#143/rag#55/artifacts#53/code#215/doc#41). MLX bare-metal via
host.docker.internal:11434. See architecture/fusion-deploy-container-multinode-0901.md."
```

## Task 2: Smoke harness — precheck + cluster-plane validation

**Files:**
- Create: `fusion-supervisor/scripts/smoke-container-multinode.sh`
- Create: `fusion-supervisor/scripts/smoke-container-multinode.test.sh`

**Interfaces:**
- Consumes: `FUSION_CLUSTER_TOKEN`, `FUSION_MLX_API_KEY` from `fusion-multi-node/.env` (base compose's `:?` already enforces, but harness prechecks independently for early failure + clean log). base compose `master` service → host-mapped `:11452`.
- Produces: `precheck()` + `cluster_up()` functions. Log file path printed at start. `teardown()` registered in `trap` (Tasks 3-5 extend). Later tasks call `cluster_up` as prerequisite.

- [ ] **Step 1: Write the failing test (precheck pure logic, no Docker)**

Create `scripts/smoke-container-multinode.test.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
# unit-test precheck() without Docker — env presence + token-nonempty guards.
# sources the harness, overrides helper that checks MLX (skip), runs precheck with
# crafted env, asserts exit codes + log markers.
cd "$(dirname "$0")/.."
HARNESS=scripts/smoke-container-multinode.sh
test -f "$HARNESS" || { echo "FAIL: harness missing"; exit 1; }

PASS=0; FAIL=0
assert_exit() { # $1=expected_rc $2=label  (uses last command's rc via $?)
    local exp="$1" label="$2" got=$?
    if [ "$got" -eq "$exp" ]; then echo "PASS: $label"; PASS=$((PASS+1));
    else echo "FAIL: $label (expected rc=$exp got rc=$got)"; FAIL=$((FAIL+1)); fi
}

# Case A: valid env → precheck passes
export FUSION_CLUSTER_TOKEN="t-ok-$(echo $$)secret"
export FUSION_MLX_API_KEY="k-ok-$(echo $$)key"
export FUSION_MULTI_NODE_DIR="$(cd ../fusion-multi-node && pwd)"
export SMOKE_LOG_DIR="$(mktemp -d)"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
# stub MLX liveness AFTER sourcing harness (harness defines real one — override it)
mlx_liveness() { return 0; }
precheck >/dev/null 2>&1; assert_exit 0 "precheck valid env"

# Case B: empty token → precheck fails
export FUSION_CLUSTER_TOKEN=""
precheck >/dev/null 2>&1; assert_exit 1 "precheck empty token rejected"

# Case C: missing MLX_API_KEY → precheck fails
unset FUSION_MLX_API_KEY
export FUSION_CLUSTER_TOKEN="t-ok"
precheck >/dev/null 2>&1; assert_exit 1 "precheck missing MLX_API_KEY rejected"

rm -rf "$SMOKE_LOG_DIR"
echo "---"; echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
```

`chmod +x scripts/smoke-container-multinode.test.sh`.

- [ ] **Step 2: Run test to verify it fails**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: FAIL with "harness missing" (harness not created yet).

- [ ] **Step 3: Write minimal implementation — harness with precheck + cluster_up**

Create `scripts/smoke-container-multinode.sh`:

```bash
#!/usr/bin/env bash
# smoke harness for container multi-node deploy (design §5.1).
# 5 steps: precheck → cluster up → business up → routed inference → auth proof.
# teardown via trap on EXIT/INT/TERM.
set -euo pipefail

# self-test hook: source-only mode skips daemon steps (unit tests call precheck).
if [ "${1:-}" = "--self-test-only" ]; then return 0 2>/dev/null || exit 0; fi

LOG_DIR="${SMOKE_LOG_DIR:-$(mktemp -d /tmp/fusion-smoke.XXXXXX)}"
LOG="$LOG_DIR/smoke.log"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
die() { log "FAIL: $*"; exit 1; }

BASE_DIR="${FUSION_MULTI_NODE_DIR:-$(cd "$(dirname "$0")/../../fusion-multi-node" 2>/dev/null && pwd)}"
BASE_COMPOSE="$BASE_DIR/docker-compose.yml"
OVERLAY="$(cd "$(dirname "$0")/.." && pwd)/docker-compose.business.yml"
COMPOSE="docker compose -f $BASE_COMPOSE -f $OVERLAY"
CLUSTER_ONLY="docker compose -f $BASE_COMPOSE"

# precheck: env shape + MLX liveness. Pure-ish (no compose), testable with stub.
mlx_liveness() {
    curl -sf "http://localhost:11434/v1/models" >/dev/null 2>&1 \
        || { log "FAIL: MLX not reachable at localhost:11434 — run ~/claude-home/fusion-mlx/start.sh start"; return 1; }
}
precheck() {
    log "STEP 1: precheck"
    [ -f "$BASE_COMPOSE" ] || die "base compose missing: $BASE_COMPOSE"
    [ -f "$OVERLAY" ] || die "overlay missing: $OVERLAY"
    [ -n "${FUSION_CLUSTER_TOKEN:-}" ] || { log "FAIL: FUSION_CLUSTER_TOKEN empty — set in $BASE_DIR/.env"; return 1; }
    [ -n "${FUSION_MLX_API_KEY:-}" ] || { log "FAIL: FUSION_MLX_API_KEY empty — set in $BASE_DIR/.env (fusion-mlx api_key)"; return 1; }
    mlx_liveness || return 1
    log "  token set, MLX live, compose files present"
}

# cluster_up: base compose only — Master + Agent healthy.
wait_healthy() { # $1=compose-cmd $2=service $3=max-wait-s
    local cmd="$1" svc="$2" max="${3:-60}" i=0
    while [ $i -lt $max ]; do
        local st; st=$($cmd ps --format '{{.Health}}' "$svc" 2>/dev/null || echo "")
        [ "$st" = "healthy" ] && { log "  $svc healthy (${i}s)"; return 0; }
        sleep 1; i=$((i+1))
    done
    die "$svc not healthy within ${max}s (last: $st)"
}
cluster_up() {
    log "STEP 2: cluster plane (base)"
    $CLUSTER_ONLY up -d master agent >/dev/null 2>&1 || die "cluster up failed"
    wait_healthy "$CLUSTER_ONLY" master 60
    wait_healthy "$CLUSTER_ONLY" agent 90
    curl -sf -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" http://127.0.0.1:11452/api/health >/dev/null \
        || die "master /api/health unreachable (token?)"
    curl -sf -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" http://127.0.0.1:11452/api/nodes | grep -q online \
        || die "no agent online in /api/nodes"
    log "  master :11452 healthy, agent online"
}

teardown() {
    log "TEARDOWN"
    $CLUSTER_ONLY down --remove-orphans >/dev/null 2>&1 || true
}
trap teardown EXIT INT TERM

# main (extended by Tasks 3-5)
precheck
cluster_up
log "smoke: cluster plane up — remaining steps added by Tasks 3-5"
log "see $LOG"
```

`chmod +x scripts/smoke-container-multinode.sh`.

- [ ] **Step 4: Run unit test to verify precheck passes**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: PASS — `PASS=3 FAIL=0` (3 precheck cases: valid env, empty token rejected, missing key rejected).
(Cumulative: later tasks append cases — Task 3→6, Task 4→9, Task 5→12.)

- [ ] **Step 5: Run harness against live cluster (integration)**

Precondition: `~/claude-home/fusion-mlx/start.sh start` (MLX live), `fusion-multi-node/.env` has `FUSION_CLUSTER_TOKEN` + `FUSION_MLX_API_KEY=dahai168`.
Run: `cd fusion-multi-node && set -a && source .env && set +a && cd - && bash scripts/smoke-container-multinode.sh`
Expected: log shows STEP 1 precheck pass + STEP 2 cluster plane up (master healthy, agent online). Exits 0, teardown tears down.

- [ ] **Step 6: Commit**

```bash
git add scripts/smoke-container-multinode.sh scripts/smoke-container-multinode.test.sh
git commit -m "feat(smoke): add multi-node smoke harness — precheck + cluster plane

scripts/smoke-container-multinode.sh: design §5.1 steps 1-2. precheck validates
env (token/api_key non-empty) + MLX liveness. cluster_up brings base compose
master+agent, waits healthy, curls /api/health + /api/nodes. trap teardown.
Unit test (no Docker) covers precheck env-shape guards. Integration needs MLX
bare-metal + .env. Tasks 3-5 extend with business/inference/auth steps."
```

## Task 3: Smoke harness — business-plane validation (model-hub + cowork)

**Files:**
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.sh` (add `business_up()` between `cluster_up` and `teardown` call; extend main)
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.test.sh` (add health-endpoint-mapping unit case — no Docker)

**Interfaces:**
- Consumes: overlay `docker-compose.business.yml` (Task 1) services `fusion-model-hub` (`:11444/api/v1/system/health`) + `fusion-cowork` (`:11438/health`). `COMPOSE` merge var defined in Task 2.
- Produces: `business_up()` called by Tasks 4-5 as prerequisite. `probe_health()` helper (port→path map) reusable by Task 7's 5 new services.

- [ ] **Step 1: Write the failing test (health-path mapping, pure)**

Append to `scripts/smoke-container-multinode.test.sh`, before the final summary block (`echo "---"`):

```bash
# Case D: probe_health maps port→path correctly (pure, no network)
export FUSION_CLUSTER_TOKEN="t-ok" FUSION_MLX_API_KEY="k-ok"
export FUSION_MULTI_NODE_DIR="$(cd ../fusion-multi-node && pwd)"
export SMOKE_LOG_DIR="$(mktemp -d)"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
# model-hub :11444 → /api/v1/system/health ; cowork :11438 → /health
[ "$(probe_path 11444)" = "/api/v1/system/health" ] && { echo "PASS: probe_path 11444"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path 11444 got '$(probe_path 11444)'"; FAIL=$((FAIL+1)); }
[ "$(probe_path 11438)" = "/health" ] && { echo "PASS: probe_path 11438"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path 11438 got '$(probe_path 11438)'"; FAIL=$((FAIL+1)); }
# unknown port → fallback /health
[ "$(probe_path 99999)" = "/health" ] && { echo "PASS: probe_path fallback"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path fallback got '$(probe_path 99999)'"; FAIL=$((FAIL+1)); }
rm -rf "$SMOKE_LOG_DIR"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: FAIL — `probe_path: command not found` (helper not defined yet).

- [ ] **Step 3: Write minimal implementation — add probe_path + business_up**

In `scripts/smoke-container-multinode.sh`, add `probe_path()` after `wait_healthy()`:

```bash
# port → health path. authoritative source: each service Dockerfile HEALTHCHECK.
# unknown → /health fallback.
probe_path() { # $1=port
    case "$1" in
        11444) echo "/api/v1/system/health" ;;  # fusion-model-hub
        11436) echo "/health" ;;                # fusion-rag (Task 7)
        11438) echo "/health" ;;                # fusion-cowork
        11441|11449|11432|11451) echo "/health" ;;  # code/doc/gateway/artifacts (Task 7)
        *) echo "/health" ;;
    esac
}
probe_health() { # $1=port
    local p="$1" path; path=$(probe_path "$p")
    curl -sf "http://127.0.0.1:$p$path" >/dev/null 2>&1 \
        || die "business service :$p$path unreachable"
}
business_up() {
    log "STEP 3: business plane (overlay)"
    $COMPOSE up -d fusion-model-hub fusion-cowork >/dev/null 2>&1 \
        || die "business overlay up failed"
    wait_healthy "$COMPOSE" fusion-model-hub 90
    wait_healthy "$COMPOSE" fusion-cowork 90
    probe_health 11444
    probe_health 11438
    log "  model-hub :11444 + cowork :11438 healthy"
}
```

Extend `main` — replace the placeholder line `log "smoke: cluster plane up — remaining steps added by Tasks 3-5"` with:

```bash
business_up
log "smoke: cluster + business planes up — inference/auth steps added by Tasks 4-5"
```

- [ ] **Step 4: Run unit test to verify probe_path passes**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: PASS — `PASS=6 FAIL=0` (3 precheck + 3 probe_path).

- [ ] **Step 5: Run harness against live cluster (integration)**

Precondition: MLX live, `.env` set (same as Task 2 Step 5).
Run: `cd fusion-multi-node && set -a && source .env && set +a && cd - && bash scripts/smoke-container-multinode.sh`
Expected: STEP 1 precheck + STEP 2 cluster + STEP 3 business (model-hub + cowork healthy). Exits 0, teardown tears down both planes via `--remove-orphans`.

- [ ] **Step 6: Commit**

```bash
git add scripts/smoke-container-multinode.sh scripts/smoke-container-multinode.test.sh
git commit -m "feat(smoke): add business-plane validation (model-hub + cowork)

business_up: brings overlay services, waits healthy, probes :11444
(/api/v1/system/health) + :11438 (/health). probe_path maps port→health path
from each Dockerfile HEALTHCHECK (reused by Task 7's 5 services). teardown
already uses --remove-orphans so overlay services tear down with cluster."
```

## Task 4: Smoke harness — routed inference proof (core)

**Files:**
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.sh` (add `routed_inference()` after `business_up()`; extend main)
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.test.sh` (add task-status extraction unit case)

**Interfaces:**
- Consumes: Master `/api/tasks/submit` (POST `TaskSubmitRequest{name, model_name, task_type:"inference", prompt, max_tokens}` → `{task_id, status, queued?}`, 202 if queued else 200). Poll `/api/tasks/{task_id}` → `_task_to_resp` `{status:"completed"|"failed"|..., result, error}`. Model `mlx-community-Llama-3.2-1B-Instruct-4bit` (cached at `~/.fusion-mlx/models`, confirmed present). All confirmed from `fusion-multi-node/server/master_server.py:905,1016,2169`.
- Produces: `routed_inference()` — the core proof that the full chain works (container→bridge→master→agent→`host.docker.internal`→MLX). Polls until `COMPLETED` or timeout.

- [ ] **Step 1: Write the failing test (status extraction from poll response, pure)**

Append to `scripts/smoke-container-multinode.test.sh`, before final summary:

```bash
# Case E: extract_status parses poll JSON correctly (pure, no network)
export FUSION_CLUSTER_TOKEN="t-ok" FUSION_MLX_API_KEY="k-ok"
export FUSION_MULTI_NODE_DIR="$(cd ../fusion-multi-node && pwd)"
export SMOKE_LOG_DIR="$(mktemp -d)"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
[ "$(extract_status '{"status":"completed","result":{"text":"hi"}}')" = "completed" ] \
    && { echo "PASS: extract_status completed"; PASS=$((PASS+1)); } \
    || { echo "FAIL: extract_status completed"; FAIL=$((FAIL+1)); }
[ "$(extract_status '{"status":"failed","error":"oom"}')" = "failed" ] \
    && { echo "PASS: extract_status failed"; PASS=$((PASS+1)); } \
    || { echo "FAIL: extract_status failed"; FAIL=$((FAIL+1)); }
[ "$(extract_status '{}' 2>/dev/null)" = "" ] \
    && { echo "PASS: extract_status empty"; PASS=$((PASS+1)); } \
    || { echo "FAIL: extract_status empty got '$(extract_status '{}' 2>/dev/null)'"; FAIL=$((FAIL+1)); }
rm -rf "$SMOKE_LOG_DIR"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: FAIL — `extract_status: command not found`.

- [ ] **Step 3: Write minimal implementation — add extract_status + routed_inference**

In `scripts/smoke-container-multinode.sh`, add after `probe_health()`:

```bash
# extract .status from poll JSON (python3 for robust parse; jq optional).
extract_status() { # $1=json
    python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))" <<<"$1" 2>/dev/null || echo ""
}
# routed_inference: core proof — Master→Agent→host.docker.internal→MLX.
SMOKE_MODEL="${SMOKE_MODEL:-mlx-community-Llama-3.2-1B-Instruct-4bit}"
routed_inference() {
    log "STEP 4: routed inference (core proof) — model=$SMOKE_MODEL"
    local body; body=$(cat <<JSON
{"name":"smoke-inference","model_name":"$SMOKE_MODEL","task_type":"inference","prompt":"Say hello in one word.","max_tokens":16}
JSON
)
    local resp rc task_id
    resp=$(curl -s -w '\n%{http_code}' -X POST -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" \
        -H "Content-Type: application/json" -d "$body" http://127.0.0.1:11452/api/tasks/submit)
    rc=$(echo "$resp" | tail -1)
    body=$(echo "$resp" | sed '$d')
    [ "$rc" = "200" ] || [ "$rc" = "202" ] || die "submit failed (http $rc): $body"
    task_id=$(python3 -c "import sys,json; print(json.load(sys.stdin).get('task_id',''))" <<<"$body" 2>/dev/null)
    [ -n "$task_id" ] || die "submit returned no task_id: $body"
    log "  submitted task_id=$task_id (http $rc)"
    # poll until completed/failed or timeout (lean: single 1B inference, 120s budget)
    local i=0 st=""
    while [ $i -lt 120 ]; do
        sleep 2; i=$((i+2))
        st=$(extract_status "$(curl -s -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" \
            http://127.0.0.1:11452/api/tasks/$task_id)")
        case "$st" in
            completed) log "  task $task_id COMPLETED (${i}s) — full chain proven"; return 0;;
            failed) die "task $task_id FAILED — check agent log + MLX reachability";;
        esac
    done
    die "task $task_id timeout after ${i}s (last status: $st)"
}
```

Extend `main` — replace `log "smoke: cluster + business planes up — inference/auth steps added by Tasks 4-5"` with:

```bash
routed_inference
log "smoke: inference proven — auth step added by Task 5"
```

- [ ] **Step 4: Run unit test to verify extract_status passes**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: PASS — `PASS=9 FAIL=0` (3 precheck + 3 probe_path + 3 extract_status).

- [ ] **Step 5: Run harness end-to-end (integration)**

Precondition: MLX live with `$SMOKE_MODEL` loaded. If MLX down: `~/claude-home/fusion-mlx/start.sh start`, then load model via its API (per fusion-mlx docs) or confirm `curl http://localhost:11434/v1/models` lists it. `.env` set.
Run: `cd fusion-multi-node && set -a && source .env && set +a && cd - && bash scripts/smoke-container-multinode.sh`
Expected: STEP 1-3 pass, STEP 4 submits → polls → `task ... COMPLETED` within 120s. Exits 0, teardown.

- [ ] **Step 6: Commit**

```bash
git add scripts/smoke-container-multinode.sh scripts/smoke-container-multinode.test.sh
git commit -m "feat(smoke): add routed-inference proof (core chain)

routed_inference: POST /api/tasks/submit {name,model_name,task_type=inference,
prompt,max_tokens=16} → poll /api/tasks/{id} until completed/failed (120s).
Proves full chain container→bridge→master→agent→host.docker.internal→MLX.
Model mlx-community-Llama-3.2-1B-Instruct-4bit (cached). extract_status unit-tested.
Schema verified against master_server.py:905/1016/2169."
```

## Task 5: Smoke harness — auth handshake proof

**Files:**
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.sh` (add `auth_proof()` after `routed_inference()`; extend main + final pass-banner)
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.test.sh` (add auth-assertion unit case)

**Interfaces:**
- Consumes: Master `/api/health` (GET — token-exempt, returns 200 regardless, confirmed route at `master_server.py:512`). `/api/nodes` (GET — requires `Bearer` token, returns 401 without). `BearerAuthMiddleware` enforces. Design §4.1 Layer 1: token always-on, fail-closed.
- Produces: `auth_proof()` — final smoke step. After it, main prints `SMOKE PASS` banner + exits 0 (teardown via trap). mTLS path documented as opt-in (not run — design §5.1 step 5 says lean smoke uses mTLS-off + token-only).

- [ ] **Step 1: Write the failing test (http-code classify, pure)**

Append to `scripts/smoke-container-multinode.test.sh`, before final summary:

```bash
# Case F: is_exempt/is_protected classify routes for auth proof (pure)
export FUSION_CLUSTER_TOKEN="t-ok" FUSION_MLX_API_KEY="k-ok"
export FUSION_MULTI_NODE_DIR="$(cd ../fusion-multi-node && pwd)"
export SMOKE_LOG_DIR="$(mktemp -d)"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
is_exempt /api/health && { echo "PASS: /api/health exempt"; PASS=$((PASS+1)); } \
    || { echo "FAIL: /api/health should be exempt"; FAIL=$((FAIL+1)); }
is_protected /api/nodes && { echo "PASS: /api/nodes protected"; PASS=$((PASS+1)); } \
    || { echo "FAIL: /api/nodes should be protected"; FAIL=$((FAIL+1)); }
! is_exempt /api/nodes && { echo "PASS: /api/nodes not exempt"; PASS=$((PASS+1)); } \
    || { echo "FAIL: /api/nodes wrongly exempt"; FAIL=$((FAIL+1)); }
rm -rf "$SMOKE_LOG_DIR"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: FAIL — `is_exempt: command not found`.

- [ ] **Step 3: Write minimal implementation — add is_exempt/is_protected + auth_proof**

In `scripts/smoke-container-multinode.sh`, add after `extract_status()`:

```bash
# token-exempt routes (BearerAuthMiddleware whitelist) — design §4.1.
is_exempt() { # $1=path
    case "$1" in
        /api/health|/health|/api/health/deep|/health/deep) return 0;;
        *) return 1;;
    esac
}
is_protected() { ! is_exempt "$1"; }
auth_proof() {
    log "STEP 5: auth handshake proof (cluster token, mTLS-off — lean)"
    # exempt route: no token → 200 (proves middleware skips it, not that auth is off)
    local rc_exempt; rc_exempt=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:11452/api/health)
    [ "$rc_exempt" = "200" ] || die "exempt /api/health should be 200 without token, got $rc_exempt"
    log "  /api/health no-token → $rc_exempt (exempt, as designed)"
    # protected route: no token → 401 (proves BearerAuthMiddleware enforces)
    local rc_denied; rc_denied=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:11452/api/nodes)
    [ "$rc_denied" = "401" ] || die "protected /api/nodes should be 401 without token, got $rc_denied"
    log "  /api/nodes no-token → $rc_denied (token enforced, fail-closed)"
    # mTLS: documented, not run (lean = mTLS-off). design §5.1 step 5.
    log "  mTLS path: opt-in, not exercised in lean smoke (see design §4.1 Layer 2)"
}
```

Extend `main` — replace `log "smoke: inference proven — auth step added by Task 5"` with:

```bash
auth_proof
log "===== SMOKE PASS: topology up + 1 routed inference + auth handshake ====="
log "log: $LOG"
```

- [ ] **Step 4: Run unit test to verify auth classifiers pass**

Run: `bash scripts/smoke-container-multinode.test.sh`
Expected: PASS — `PASS=12 FAIL=0` (3+3+3+3).

- [ ] **Step 5: Run harness end-to-end (full smoke)**

Precondition: MLX live with smoke model loaded, `.env` set.
Run: `cd fusion-multi-node && set -a && source .env && set +a && cd - && bash scripts/smoke-container-multinode.sh`
Expected: all 5 steps pass, ends with `SMOKE PASS` banner. Exits 0, teardown tears down both planes.

- [ ] **Step 6: Commit**

```bash
git add scripts/smoke-container-multinode.sh scripts/smoke-container-multinode.test.sh
git commit -m "feat(smoke): add auth handshake proof + final pass banner

auth_proof: /api/health no-token → 200 (exempt), /api/nodes no-token → 401
(BearerAuthMiddleware enforced, fail-closed). mTLS path documented not run
(lean smoke = mTLS-off + token-only, design §5.1 step 5 / §4.1 Layer 2).
Harness now runs all 5 design §5.1 steps end-to-end, prints SMOKE PASS banner.
is_exempt/is_protected unit-tested against master_server.py:512 route map."
```

## Task 6: Cleanup, README, finish

**Files:**
- Modify: `fusion-supervisor/README.md` (add "Container Multi-Node Deploy" section)
- Verify: `fusion-supervisor/scripts/smoke-container-multinode.sh` teardown completeness (process data cleanup per CLAUDE.md)

**Interfaces:**
- Consumes: all prior tasks — overlay compose (Task 1), harness with 5 smoke steps (Tasks 2-5).
- Produces: documented deploy section in README. No new files.

- [ ] **Step 1: Read current README tail (find insertion point)**

Run: `tail -20 fusion-supervisor/README.md`
Identify the last section heading — insert the new section after it, before any "License"/end.

- [ ] **Step 2: Write README section**

Append to `fusion-supervisor/README.md`:

```markdown

## Container Multi-Node Deploy

Containerized multi-node commercial-deployment topology — extends
`architecture/fusion-deploy-container-multinode-0901.md`. Layered Docker
Compose: base cluster plane (`fusion-multi-node/docker-compose.yml`, network
pin #59 landed) + business-plane overlay (`docker-compose.business.yml` here).

**What's containerized:** cluster plane (Master + Agent) + HTTP business plane
(model-hub, cowork now; gateway/rag/artifacts/code/doc gated on upstream
Dockerfile PRs). **What stays bare-metal:** MLX (`localhost:11434`, Metal),
UDS guardian plane (memory/speech — UDS doesn't cross container boundary).

### Prerequisites

1. MLX running bare-metal: `~/claude-home/fusion-mlx/start.sh start`
2. `fusion-multi-node/.env` set: `FUSION_CLUSTER_TOKEN` (strong random),
   `FUSION_MLX_API_KEY=dahai168` (matches fusion-mlx api_key)
3. Smoke model cached: `mlx-community-Llama-3.2-1B-Instruct-4bit`
   (download via `HF_MIRROR=https://hf-mirror.com` if missing), loaded in MLX
4. Docker Desktop running (v29.4+)

### Run smoke validation

```bash
cd fusion-supervisor
cd ../fusion-multi-node && set -a && source .env && set +a && cd -
bash scripts/smoke-container-multinode.sh
```

Runs 5 steps (design §5.1): precheck → cluster up → business up → routed
inference (Master→Agent→MLX) → auth handshake. Logs to
`/tmp/fusion-smoke.*/smoke.log`. Pass banner = `SMOKE PASS`.

### Bring up topology without smoke

```bash
cd fusion-multi-node
docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml up -d
```

Teardown: `docker compose -f docker-compose.yml -f ../fusion-supervisor/docker-compose.business.yml down --remove-orphans`

### Cleanup (process data)

Smoke harness auto-teardowns via `trap` (no persistent test data kept — only
`/tmp/fusion-smoke.*/smoke.log` remains). For manual teardown + cert cleanup
(mTLS path): `docker compose ... down` + `rm -rf fusion-multi-node/tls/`.
```

- [ ] **Step 3: Verify teardown leaves no process data**

Run: `bash scripts/smoke-container-multinode.sh` (full smoke), then check residue:
`docker ps -a --filter name=fusion | grep -v 'Exited (0)'` — should be empty (all torn down).
`docker network ls | grep fusion-cluster` — may persist (harmless, reused); note in README.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(deploy): document container multi-node deploy + smoke harness

README section: prerequisites (MLX bare-metal, .env, smoke model, Docker),
run smoke validation (5 steps per design §5.1), bring up topology without
smoke, cleanup (auto-teardown via trap, no persistent test data).
See architecture/fusion-deploy-container-multinode-0901.md."
```

- [ ] **Step 5: Final verification — full smoke + lint**

Run: `bash scripts/smoke-container-multinode.test.sh && bash scripts/smoke-container-multinode.sh`
Expected: unit tests `PASS=12 FAIL=0`, then full smoke `SMOKE PASS`. If any fail, do not finish — root-cause per CLAUDE.md ("遇到报错的用例...都要定位修复").

## Task 7: [GATED on 5 upstream PRs] Add gateway/rag/artifacts/code/doc to overlay

**Gate:** Do NOT start until all 5 upstream PRs land (user-tracked):
fusion-gateway#143, fusion-rag#55, fusion-artifacts-engine#53, fusion-code#215, fusion-doc#41.
Each repo root must have a working `Dockerfile` (verify per Step 1).

**Files:**
- Modify: `fusion-supervisor/docker-compose.business.yml` (add 5 service blocks)
- Modify: `fusion-supervisor/scripts/smoke-container-multinode.sh` (`business_up` already iterates implicitly via probe_path — extend the `up -d` line + add probe_health calls)

**Interfaces:**
- Consumes: 5 Dockerfiles landed upstream. Health paths from each Dockerfile HEALTHCHECK (verify in Step 1 — do not assume; the issues specified `200` at `/health` or `/api/health`, confirm exact). Ports from `port-registry.yaml`: gateway 11432, rag 11436, artifacts 11451, code 11441, doc 11449.
- Produces: full 7-service business plane. Design §5.1 step 3 expands to all 7. rag→memory UDS gap handled (design §2.3: rag stays bare-metal OR memory HTTP shim — see Step 3).

- [ ] **Step 1: Verify all 5 Dockerfiles landed + read their HEALTHCHECK paths**

For each repo, confirm `Dockerfile` exists and extract its health path + port:
```bash
for r in fusion-gateway fusion-rag fusion-artifacts-engine fusion-code fusion-doc; do
    echo "=== $r ==="
    [ -f ../$r/Dockerfile ] && echo "Dockerfile: present" || echo "Dockerfile: MISSING — PR not landed, abort Task 7"
    grep -nE 'EXPOSE|HEALTHCHECK|/health|/api/health' ../$r/Dockerfile 2>/dev/null | head
done
```
Record exact health path per service. If any `MISSING`, STOP — gate not met; revisit when that PR lands.

- [ ] **Step 2: Write the failing test (overlay config still reconciles with 7 services)**

The existing `scripts/test-overlay-compose.sh` (Task 1) already validates `docker compose config` reconciles. After adding 5 services, rerun it — it should still pass (no new test needed, but verify the merge includes all 7):
Add to `scripts/test-overlay-compose.sh`, after the `name: fusion-cluster` grep:

```bash
# all declared business services present in merged config (7 after Task 7)
for svc in fusion-model-hub fusion-cowork fusion-gateway fusion-rag fusion-artifacts-engine fusion-code fusion-doc; do
    docker compose -f "$BASE" -f "$OVERLAY" config 2>&1 | grep -q "  $svc:" \
        || { echo "FAIL: service $svc missing from merged config"; exit 1; }
done
echo "PASS: all 7 business services present in merged config"
```

- [ ] **Step 3: Run test to verify it fails**

Run: `bash scripts/test-overlay-compose.sh`
Expected: FAIL — services `fusion-gateway`/etc. missing (not added yet).

- [ ] **Step 4: Add 5 service blocks to overlay**

In `docker-compose.business.yml`, after the `fusion-cowork:` block (before EOF), add:

```yaml
  fusion-gateway:               # Go — Dockerfile from issue #143
    build: ../fusion-gateway
    ports: ["11432:11432"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_MLX_API_KEY: "${FUSION_MLX_API_KEY:?必须设置}"
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"

  fusion-rag:                   # py — Dockerfile from issue #55
    build: ../fusion-rag
    ports: ["11436:11436"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_MLX_API_KEY: "${FUSION_MLX_API_KEY:?必须设置}"
      # fusion-memory 走 UDS (/tmp/fusion-memory.sock), 不过容器边界;
      # 若 rag 依赖 memory: 需 memory 提供 HTTP shim, 或 rag 留裸机 (见设计 §2.3)。
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"

  fusion-artifacts-engine:      # py — Dockerfile from issue #53
    build: ../fusion-artifacts-engine
    ports: ["11451:11451"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_MLX_API_KEY: "${FUSION_MLX_API_KEY:?必须设置}"
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"

  fusion-code:                  # TS/Bun — Dockerfile from issue #215
    build: ../fusion-code
    ports: ["11441:11441"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_CODE_NO_AUTH: "1"   # smoke: token discovery across container boundary awkward (see issue #215)
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"

  fusion-doc:                   # node — Dockerfile from issue #41
    build: ../fusion-doc
    ports: ["11449:11449"]
    environment:
      FUSION_MLX_URL: "http://host.docker.internal:11434"
      FUSION_DOC_HOST: "0.0.0.0"
    extra_hosts: ["host.docker.internal:host-gateway"]
    networks: [fusion-cluster]
    restart: unless-stopped
    logging: {driver: json-file, options: {max-size: "10m", max-file: "3"}}
    mem_limit: "2g"
    cpus: "2"
```

- [ ] **Step 5: Update probe_path + business_up for 5 new services**

In `scripts/smoke-container-multinode.sh`, `probe_path()` case-list already covers 11436/11441/11449/11432/11451 (added in Task 3 proactively). Update `business_up()` — replace its body:

```bash
business_up() {
    log "STEP 3: business plane (overlay) — 7 services"
    $COMPOSE up -d fusion-model-hub fusion-cowork fusion-gateway \
        fusion-rag fusion-artifacts-engine fusion-code fusion-doc >/dev/null 2>&1 \
        || die "business overlay up failed"
    for svc in fusion-model-hub fusion-cowork fusion-gateway \
               fusion-rag fusion-artifacts-engine fusion-code fusion-doc; do
        wait_healthy "$COMPOSE" "$svc" 120
    done
    probe_health 11444  # model-hub
    probe_health 11438  # cowork
    probe_health 11432  # gateway
    probe_health 11436  # rag
    probe_health 11451  # artifacts
    probe_health 11441  # code
    probe_health 11449  # doc
    log "  all 7 business services healthy"
}
```

- [ ] **Step 6: Run test to verify overlay reconciles with 7 services**

Run: `bash scripts/test-overlay-compose.sh`
Expected: PASS — "all 7 business services present in merged config".

- [ ] **Step 7: Run full smoke (integration)**

Precondition: MLX live + smoke model loaded + `.env` set (same as prior).
Run: `cd fusion-multi-node && set -a && source .env && set +a && cd - && bash scripts/smoke-container-multinode.sh`
Expected: STEP 3 brings all 7 healthy, steps 4-5 pass, `SMOKE PASS`. If a service fails health, root-cause (CLAUDE.md) — likely wrong health path (re-verify Step 1) or service-specific env (check that repo's issue/PR).

- [ ] **Step 8: Commit**

```bash
git add docker-compose.business.yml scripts/smoke-container-multinode.sh scripts/test-overlay-compose.sh
git commit -m "feat(deploy): add 5 business services to overlay (gated PRs landed)

gateway#143/rag#55/artifacts#53/code#215/doc#41 Dockerfiles landed upstream.
Overlay now 7 services. business_up brings all 7, probes each health path.
rag UDS-memory gap noted (design §2.3). code runs FUSION_CODE_NO_AUTH=1 for
smoke. Full smoke now validates complete business plane. Design doc upgrades
from 'partial' to 'full validation'."
```

---

## Self-Review

**1. Spec coverage** (design `architecture/fusion-deploy-container-multinode-0901.md`):
- §1 目标 (containerize cluster+business, smoke-level, M5 128G) → Global Constraints + Goal. ✅
- §2.2 layered compose (base + overlay) → Task 1. ✅
- §2.3 组网契约 (host.docker.internal, bridge DNS, UDS不通) → Global Constraints + overlay env + rag UDS note Task 7. ✅
- §2.4 port-registry unchanged → Global Constraints. ✅
- §3.1 base network pin → upstream #59 (landed), noted. ✅
- §3.2 overlay 7 services → Task 1 (2) + Task 7 (5). ✅
- §3.3 资源预算 2GB/2cpu → overlay mem_limit/cpus every service. ✅
- §4.1 3-layer security (token always-on, mTLS opt-in, RBAC documented) → Task 5 (token proof) + Global Constraints (mTLS-off lean, RBAC not exercised). ✅
- §4.3 治理 (supervisor code untouched, 5 Dockerfiles upstream) → Global Constraints + Task 7 gate. ✅
- §5.1 smoke 5 steps → Tasks 2-5 (precheck, cluster, business, inference, auth). ✅
- §5.2 故障模式 (restart unless-stopped, mem_limit, depends_on) → overlay `restart` + `mem_limit`; base compose already has depends_on/healthcheck (audited, not re-specified). ✅
- §5.4 cleanup (down, delete tls, keep logs) → Task 6 README + trap teardown. ✅
- §6 Phase 2 upstream 6 issues → dependency table + Task 7 gate. ✅

**2. Placeholder scan:** Searched for TBD/TODO/"fill in"/"similar to"/"implement later" — none remain. All code blocks concrete (verified schema against `master_server.py:905/1016/2169`, health paths against Dockerfiles, model name against `~/.fusion-mlx/models`).

**3. Type/function consistency:**
- `probe_path(port)` defined Task 3, used Task 3+7. Signature stable. ✅
- `probe_health(port)` defined Task 3, used Task 3+7. ✅
- `extract_status(json)` defined Task 4, used Task 4. ✅
- `is_exempt/is_protected(path)` defined Task 5, used Task 5 test. ✅
- `wait_healthy(compose-cmd, service, max-wait)` defined Task 2, used Task 2/3/7. ✅
- `business_up()` defined Task 3, **rewritten** Task 7 (2→7 services) — flagged as intentional rewrite, not a bug. ✅
- `precheck()` / `cluster_up()` / `routed_inference()` / `auth_proof()` — stable. ✅
- Test PASS counts cumulative (3→6→9→12) — noted in Task 2 Step 4. ✅
- Self-test stub `mlx_liveness` override placed AFTER source (fixed: harness real def would shadow pre-source export). ✅
- Service names match between overlay YAML + `business_up()` + `probe_path()` case-list + test-compose service grep. ✅

**4. Governance check:** All modified files under `fusion-supervisor/`. No edits to fusion-multi-node/gateway/rag/artifacts/code/doc. Upstream via issue→PR (table). ✅

Plan is internally consistent, covers the full spec, no placeholders.
