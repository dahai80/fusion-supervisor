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
log "smoke: cluster + business planes up — inference/auth steps added by Tasks 4-5"
log "see $LOG"
