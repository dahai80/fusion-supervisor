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

# self-test hook: source-only mode defines functions but skips main daemon steps.
# (unit tests source the harness, override mlx_liveness, then call precheck directly.)
if [ "${1:-}" = "--self-test-only" ]; then return 0 2>/dev/null || exit 0; fi

# main (extended by Tasks 3-5)
precheck
cluster_up
log "smoke: cluster plane up — remaining steps added by Tasks 3-5"
log "see $LOG"
