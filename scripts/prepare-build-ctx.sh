#!/usr/bin/env bash
# Stage a lean Docker build context for the business-plane overlay.
#
# Why: fusion-model-hub/Dockerfile and fusion-cowork/Dockerfile both COPY paths
# that live at the monorepo root (requirements.lock, fusion-core/, fusion-cowork/),
# so their build context MUST be the monorepo root. But the monorepo root is
# ~200G (.venv 3.8G, sibling projects 40G+ each) with no root .dockerignore — a
# root-context build would ship the whole tree to the daemon and time out.
#
# Fix (in-scope, fusion-supervisor only): assemble a staging context dir here
# with real copies of just the paths each Dockerfile COPYs, plus a .dockerignore.
# The overlay's `build.context` points at this dir; `build.dockerfile` points
# back at the real Dockerfile in the source repo. Generated dir is git-ignored.
#
# Re-run any time the staged sources change. Idempotent (rm + repopulate).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SUP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MONO_ROOT="$(cd "$SUP_ROOT/.." && pwd)"
STG="$SUP_ROOT/.build-ctx"

log() { echo "[prepare-build-ctx] $*"; }
die() { echo "[prepare-build-ctx] FAIL: $*" >&2; exit 1; }

[ -d "$MONO_ROOT/fusion-model-hub" ] || die "fusion-model-hub missing at $MONO_ROOT"
[ -d "$MONO_ROOT/fusion-cowork" ] || die "fusion-cowork missing at $MONO_ROOT"
[ -d "$MONO_ROOT/fusion-core" ] || die "fusion-core missing at $MONO_ROOT"
[ -f "$MONO_ROOT/requirements.lock" ] || die "requirements.lock missing at $MONO_ROOT"

log "staging context -> $STG"
rm -rf "$STG"
mkdir -p "$STG"

# --- model-hub needs at context root ---
#   requirements.lock (root), pyproject.toml, README.md, alembic.ini,
#   fusion_model_hub/, alembic/
cp "$MONO_ROOT/requirements.lock" "$STG/requirements.lock"
cp "$MONO_ROOT/fusion-model-hub/pyproject.toml" "$STG/pyproject.toml"
cp "$MONO_ROOT/fusion-model-hub/README.md" "$STG/README.md"
cp "$MONO_ROOT/fusion-model-hub/alembic.ini" "$STG/alembic.ini"
rsync -a \
    --exclude='__pycache__' --exclude='*.pyc' --exclude='.pytest_cache' --exclude='.ruff_cache' \
    "$MONO_ROOT/fusion-model-hub/fusion_model_hub/" "$STG/fusion_model_hub/"
rsync -a \
    --exclude='__pycache__' --exclude='*.pyc' \
    "$MONO_ROOT/fusion-model-hub/alembic/" "$STG/alembic/"

# --- cowork needs at context root: fusion-core/, fusion-cowork/ ---
rsync -a \
    --exclude='__pycache__' --exclude='*.pyc' --exclude='.pytest_cache' \
    --exclude='.venv' --exclude='.git' --exclude='.codegraph' \
    "$MONO_ROOT/fusion-core/" "$STG/fusion-core/"
rsync -a \
    --exclude='__pycache__' --exclude='*.pyc' --exclude='.pytest_cache' \
    --exclude='.venv' --exclude='.git' --exclude='.codegraph' \
    --exclude='browser' --exclude='tests' --exclude='logs' --exclude='*.egg-info' \
    --exclude='node_modules' --exclude='dist' --exclude='build' \
    "$MONO_ROOT/fusion-cowork/" "$STG/fusion-cowork/"

# .dockerignore at staging root — belt-and-suspenders on top of rsync excludes.
cat > "$STG/.dockerignore" <<'EOF'
**/.git
**/.venv
**/__pycache__
**/*.pyc
**/.pytest_cache
**/.ruff_cache
**/.codegraph
**/tests
**/browser
**/logs
**/.remember
**/*.egg-info
**/.DS_Store
**/node_modules
**/dist
**/build
EOF

log "done. staged size: $(du -sh "$STG" | awk '{print $1}')"
log "overlay build.context -> $STG (dockerfile paths stay in source repos)"
