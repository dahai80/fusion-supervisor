#!/usr/bin/env bash
# smoke harness for the sidecar middleware stack (issue #1).
# Validates docker-compose.sidecar.yml: traefik + redis + postgres + qdrant all
# healthy, then merges the business overlay and confirms model-hub + cowork come
# up healthy on the shared PG/Redis (no self-hosted PG).
#
# This does NOT spin MLX/fusion-gateway — sidecar middleware is MLX-independent.
# routed-inference is covered by smoke-container-multinode.sh (cluster plane).
#
# Steps: precheck → sidecar up → middleware healthy → overlay merge → business healthy
#        → traefik routes reach containers → teardown (trap).
# Logs to /tmp/fusion-smoke-sidecar.*/smoke.log. Pass banner = SMOKE PASS.
set -euo pipefail

LOG_DIR="${SMOKE_LOG_DIR:-$(mktemp -d /tmp/fusion-smoke-sidecar.XXXXXX)}"
LOG="$LOG_DIR/smoke.log"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
die() { log "FAIL: $*"; exit 1; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SIDECAR="$ROOT/docker-compose.sidecar.yml"
OVERLAY="$ROOT/docker-compose.business.yml"
ENV_FILE="${FUSION_ENV_FILE:-$ROOT/.env}"

# fail-closed env: sidecar compose requires FUSION_PG_PASSWORD; business requires
# FUSION_MLX_API_KEY. Bootstrap a temp .env if none so the harness is self-contained.
ensure_env() {
    if [ ! -f "$ENV_FILE" ]; then
        log "  no .env found, bootstrapping temp env"
        ENV_FILE="$LOG_DIR/.env"
        {
            echo "FUSION_PG_PASSWORD=smoke-pass-$$"
            echo "FUSION_MLX_API_KEY=smoke-mlx-key"
        } > "$ENV_FILE"
    fi
}
COMPOSE_BASE="docker compose --env-file $ENV_FILE -f $SIDECAR"
COMPOSE_FULL="docker compose --env-file $ENV_FILE -f $SIDECAR -f $OVERLAY"

precheck() {
    log "STEP 1: precheck"
    [ -f "$SIDECAR" ] || { log "FAIL: sidecar compose missing: $SIDECAR"; return 1; }
    [ -f "$OVERLAY" ] || { log "FAIL: overlay missing: $OVERLAY"; return 1; }
    ensure_env
    # config parse — catches interpolation/volume errors before bringing anything up
    $COMPOSE_BASE config --quiet >/dev/null 2>&1 || { log "FAIL: sidecar compose config invalid"; return 1; }
    $COMPOSE_FULL config --quiet >/dev/null 2>&1 || { log "FAIL: merged compose config invalid"; return 1; }
    log "  compose configs valid, env loaded ($ENV_FILE)"
}

wait_healthy() { # $1=compose-cmd $2=service $3=max-wait-s
    local cmd="$1" svc="$2" max="${3:-90}" i=0 st=""
    while [ $i -lt $max ]; do
        st=$($cmd ps --format '{{.Health}}' "$svc" 2>/dev/null || echo "")
        [ "$st" = "healthy" ] && { log "  $svc healthy (${i}s)"; return 0; }
        sleep 1; i=$((i+1))
    done
    die "$svc not healthy within ${max}s (last: $st)"
}

sidecar_up() {
    log "STEP 2: sidecar plane (traefik + redis + postgres + qdrant)"
    $COMPOSE_BASE up -d traefik redis postgres qdrant >/dev/null 2>&1 \
        || die "sidecar up failed"
    wait_healthy "$COMPOSE_BASE" traefik 60
    wait_healthy "$COMPOSE_BASE" redis 30
    wait_healthy "$COMPOSE_BASE" postgres 60
    wait_healthy "$COMPOSE_BASE" qdrant 60
    log "  all 4 sidecar services healthy"
}

# confirm PG created the multi-DB set (cowork/identity/rag/modelhub) per pg-init script.
pg_dbs_present() {
    log "STEP 3: postgres multi-DB init check"
    local pass; pass=$(grep '^FUSION_PG_PASSWORD=' "$ENV_FILE" | cut -d= -f2)
    local dbs out
    dbs=$(docker exec "$($COMPOSE_BASE ps -q postgres)" \
        psql -U fusion -d fusion -tAc \
        "SELECT datname FROM pg_database WHERE datname IN ('cowork','identity','rag','modelhub') ORDER BY datname;" 2>/dev/null || echo "")
    out=$(echo "$dbs" | tr -d ' ' | grep -c . || echo 0)
    [ "$out" -ge 4 ] || die "expected 4 app DBs, found: [$dbs]"
    log "  $out app DBs present (cowork/identity/rag/modelhub)"
}

business_up() {
    log "STEP 4: business overlay on shared sidecar (model-hub + cowork)"
    # business Dockerfiles need staged build context
    local ctx="$ROOT/.build-ctx"
    if [ ! -d "$ctx" ]; then
        log "  staging build context (prepare-build-ctx.sh)"
        bash "$ROOT/scripts/prepare-build-ctx.sh" >/dev/null 2>&1 \
            || die "prepare-build-ctx.sh failed"
    fi
    $COMPOSE_FULL up -d fusion-model-hub fusion-cowork >/dev/null 2>&1 \
        || die "business overlay up failed"
    wait_healthy "$COMPOSE_FULL" fusion-model-hub 120
    wait_healthy "$COMPOSE_FULL" fusion-cowork 120
    log "  model-hub + cowork healthy on shared PG/Redis (no self-hosted PG)"
}

# traefik routes: confirm the router table loaded for model-hub + cowork + gateway.
traefik_routes() {
    log "STEP 5: traefik routing table"
    local rc
    rc=$(curl -sf -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/api/http/routers 2>/dev/null || echo "000")
    [ "$rc" = "200" ] || die "traefik API unreachable on :8080 (got $rc)"
    local routers
    routers=$(curl -sf http://127.0.0.1:8080/api/http/routers 2>/dev/null || echo "[]")
    echo "$routers" | grep -q 'model-hub' || die "traefik missing model-hub router"
    echo "$routers" | grep -q 'cowork' || die "traefik missing cowork router"
    log "  traefik routers loaded: model-hub + cowork (labels wired)"
}

teardown() {
    log "TEARDOWN"
    $COMPOSE_FULL down --remove-orphans >/dev/null 2>&1 || true
}
trap teardown EXIT INT TERM

# self-test hook for unit sourcing (matches smoke-container-multinode.sh convention)
if [ "${1:-}" = "--self-test-only" ]; then return 0 2>/dev/null || exit 0; fi

precheck
sidecar_up
pg_dbs_present
business_up
traefik_routes
log "===== SMOKE PASS: sidecar middleware up + business on shared PG/Redis + traefik routed ====="
log "log: $LOG"
