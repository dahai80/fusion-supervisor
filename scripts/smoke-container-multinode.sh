#!/usr/bin/env bash
# smoke harness for container multi-node deploy (design §5.1).
# 5 steps: precheck → cluster up → business up → routed inference → auth proof.
# teardown via trap on EXIT/INT/TERM.
set -euo pipefail

LOG_DIR="${SMOKE_LOG_DIR:-$(mktemp -d /tmp/fusion-smoke.XXXXXX)}"
LOG="$LOG_DIR/smoke.log"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
die() { log "FAIL: $*"; exit 1; }

BASE_DIR="${FUSION_MULTI_NODE_DIR:-$(cd "$(dirname "$0")/../../fusion-multi-node" 2>/dev/null && pwd)}"
BASE_COMPOSE="$BASE_DIR/docker-compose.yml"
OVERLAY="$(cd "$(dirname "$0")/.." && pwd)/docker-compose.business.yml"
COMPOSE="docker compose -f $BASE_COMPOSE -f $OVERLAY"
CLUSTER_ONLY="docker compose -f $BASE_COMPOSE"
PREPARE_CTX="$(cd "$(dirname "$0")" && pwd)/prepare-build-ctx.sh"

# precheck: env shape + MLX liveness. Pure-ish (no compose), testable with stub.
# MLX /health is token-exempt (200) — /v1/models requires API key (401), wrong for liveness.
mlx_liveness() {
    curl -sf "http://localhost:11434/health" >/dev/null 2>&1 \
        || { log "FAIL: MLX not reachable at localhost:11434 — run ~/claude-home/fusion-mlx/start.sh start"; return 1; }
}
precheck() {
    log "STEP 1: precheck"
    [ -f "$BASE_COMPOSE" ] || die "base compose missing: $BASE_COMPOSE"
    [ -f "$OVERLAY" ] || die "overlay missing: $OVERLAY"
    # business-plane Dockerfiles COPY root-relative paths — overlay build.context
    # is a staged lean dir (.build-ctx/). Ensure it exists before compose build.
    if [ -x "$PREPARE_CTX" ] && [ ! -d "$(cd "$(dirname "$0")/.." && pwd)/.build-ctx" ]; then
        log "  staging build context (prepare-build-ctx.sh)"
        "$PREPARE_CTX" >/dev/null 2>&1 || die "prepare-build-ctx.sh failed"
    fi
    [ -n "${FUSION_CLUSTER_TOKEN:-}" ] || { log "FAIL: FUSION_CLUSTER_TOKEN empty — set in $BASE_DIR/.env"; return 1; }
    [ -n "${FUSION_MLX_API_KEY:-}" ] || { log "FAIL: FUSION_MLX_API_KEY empty — set in $BASE_DIR/.env (fusion-mlx api_key)"; return 1; }
    mlx_liveness || return 1
    log "  token set, MLX live, compose files present"
}

# cluster_up: base compose only — Master healthy + Agent online.
wait_healthy() { # $1=compose-cmd $2=service $3=max-wait-s
    local cmd="$1" svc="$2" max="${3:-60}" i=0
    while [ $i -lt $max ]; do
        local st; st=$($cmd ps --format '{{.Health}}' "$svc" 2>/dev/null || echo "")
        [ "$st" = "healthy" ] && { log "  $svc healthy (${i}s)"; return 0; }
        sleep 1; i=$((i+1))
    done
    die "$svc not healthy within ${max}s (last: $st)"
}
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
# extract .status from poll JSON (python3 for robust parse; jq optional).
extract_status() { # $1=json
    python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))" <<<"$1" 2>/dev/null || echo ""
}
# token-exempt routes (BearerAuthMiddleware whitelist) — design §4.1.
is_exempt() { # $1=path
    case "$1" in
        /api/health|/health|/api/health/deep|/health/deep) return 0;;
        *) return 1;;
    esac
}
is_protected() { ! is_exempt "$1"; }
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
# agent Docker healthcheck probes /api/health/deep which hardcodes localhost:11432
# (upstream fusion-multi-node bug: ignores FUSION_MLX_URL, attr mismatch base_url vs _base_url).
# Docker health stays unhealthy despite agent registered+online. Validate via master
# /api/nodes online count — the real topology signal, not the broken healthcheck.
wait_agent_online() { # $1=max-wait-s
    local max="${1:-90}" i=0 body=""
    while [ $i -lt $max ]; do
        body=$(curl -sS -m 5 -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" http://127.0.0.1:11452/api/nodes 2>/dev/null || echo "")
        echo "$body" | grep -Eq '"online"[: ]+[1-9]' && { log "  agent online via /api/nodes (${i}s)"; return 0; }
        sleep 2; i=$((i+2))
    done
    die "agent not online in /api/nodes within ${max}s (last: $body)"
}
cluster_up() {
    log "STEP 2: cluster plane (base)"
    $CLUSTER_ONLY up -d master agent >/dev/null 2>&1 || die "cluster up failed"
    wait_healthy "$CLUSTER_ONLY" master 60
    wait_agent_online 90
    curl -sf -H "Authorization: Bearer $FUSION_CLUSTER_TOKEN" http://127.0.0.1:11452/api/health >/dev/null \
        || die "master /api/health unreachable"
    log "  master :11452 healthy, agent online"
}

teardown() {
    log "TEARDOWN"
    $CLUSTER_ONLY down --remove-orphans >/dev/null 2>&1 || true
}
trap teardown EXIT INT TERM

# self-test hook: source-only mode defines functions but skips main daemon steps.
# (unit tests source the harness, override mlx_liveness, then call precheck directly.)
if [ "${1:-}" = "--self-test-only" ]; then return 0 2>/dev/null || exit 0; fi

# main (extended by Tasks 3-5)
precheck
cluster_up
business_up
routed_inference
auth_proof
log "===== SMOKE PASS: topology up + 1 routed inference + auth handshake ====="
log "log: $LOG"
