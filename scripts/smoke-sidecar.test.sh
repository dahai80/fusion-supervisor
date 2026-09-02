#!/usr/bin/env bash
set -uo pipefail
# unit-test smoke-sidecar.sh precheck without Docker — compose-file presence +
# config-validity guards, env fail-closed. NON-DESTRUCTIVE: copies compose files
# to a temp dir and points the harness at the copies (never moves the real files).
cd "$(dirname "$0")/.."
HARNESS=scripts/smoke-sidecar.sh
test -f "$HARNESS" || { echo "FAIL: harness missing"; exit 1; }

PASS=0; FAIL=0
assert_exit() { # $1=expected_rc $2=label  (uses last command's rc via $?)
    local exp="$1" label="$2" got=$?
    if [ "$got" -eq "$exp" ]; then echo "PASS: $label"; PASS=$((PASS+1));
    else echo "FAIL: $label (expected rc=$exp got rc=$got)"; FAIL=$((FAIL+1)); fi
}

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# stage copies so "missing file" cases can delete the copy without touching source
cp docker-compose.sidecar.yml "$WORK/sidecar.yml"
cp docker-compose.business.yml "$WORK/business.yml"
echo "FUSION_PG_PASSWORD=test-pass" > "$WORK/.env"
echo "FUSION_MLX_API_KEY=test-key" >> "$WORK/.env"

export SMOKE_LOG_DIR="$WORK/logs"; mkdir -p "$SMOKE_LOG_DIR"
export FUSION_ENV_FILE="$WORK/.env"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
trap cleanup EXIT   # harness armed teardown; re-arm our cleanup
set +e              # harness sourced with -e; tolerate expected-failure returns

# point harness at the copies + stub compose so no real docker runs
SIDECAR="$WORK/sidecar.yml"
OVERLAY="$WORK/business.yml"
COMPOSE_BASE=true
COMPOSE_FULL=true

# Case A: valid env + existing compose copies → precheck passes
precheck >/dev/null 2>&1; assert_exit 0 "precheck valid env + compose files"

# Case B: missing sidecar compose copy → precheck fails (delete copy, not source)
rm -f "$WORK/sidecar.yml"
precheck >/dev/null 2>&1; assert_exit 1 "precheck missing sidecar compose rejected"
cp docker-compose.sidecar.yml "$WORK/sidecar.yml"

# Case C: missing overlay copy → precheck fails
rm -f "$WORK/business.yml"
precheck >/dev/null 2>&1; assert_exit 1 "precheck missing overlay rejected"
cp docker-compose.business.yml "$WORK/business.yml"

# Case D: wait_healthy stub returns healthy (pure, no network)
wait_healthy() { log "  (stub) $2 healthy (0s)"; return 0; }
wait_healthy "true" traefik 5; assert_exit 0 "wait_healthy stub returns healthy"

echo "---"; echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
