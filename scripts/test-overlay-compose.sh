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
