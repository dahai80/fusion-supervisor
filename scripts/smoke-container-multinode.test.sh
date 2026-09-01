#!/usr/bin/env bash
set -uo pipefail
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
set +e  # harness re-enables errexit on source; test asserts expected-failure rc's
trap - EXIT INT TERM  # unit test brings up no cluster; drop harness teardown
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

# Case D: probe_health maps port→path correctly (pure, no network)
export FUSION_CLUSTER_TOKEN="t-ok" FUSION_MLX_API_KEY="k-ok"
export FUSION_MULTI_NODE_DIR="$(cd ../fusion-multi-node && pwd)"
export SMOKE_LOG_DIR="$(mktemp -d)"
source "$HARNESS" --self-test-only >/dev/null 2>&1 || true
trap - EXIT INT TERM  # Case D re-sourced harness (re-armed teardown trap); drop it
# model-hub :11444 → /api/v1/system/health ; cowork :11438 → /health
[ "$(probe_path 11444)" = "/api/v1/system/health" ] && { echo "PASS: probe_path 11444"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path 11444 got '$(probe_path 11444)'"; FAIL=$((FAIL+1)); }
[ "$(probe_path 11438)" = "/health" ] && { echo "PASS: probe_path 11438"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path 11438 got '$(probe_path 11438)'"; FAIL=$((FAIL+1)); }
# unknown port → fallback /health
[ "$(probe_path 99999)" = "/health" ] && { echo "PASS: probe_path fallback"; PASS=$((PASS+1)); } \
    || { echo "FAIL: probe_path fallback got '$(probe_path 99999)'"; FAIL=$((FAIL+1)); }
rm -rf "$SMOKE_LOG_DIR"
echo "---"; echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
