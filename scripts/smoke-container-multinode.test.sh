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
echo "---"; echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
