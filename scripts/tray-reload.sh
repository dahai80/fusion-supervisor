#!/usr/bin/env bash
# issue #108: safe install/upgrade path for the tray LaunchAgent.
#
# Why: after `cargo build --release` replaces target/release/fusion-tray, the
# kernel may SIGKILL the new binary at spawn with "Launch Constraint Violation"
# (EXC_CRASH / CODESIGNING) — launchd's cached signature/constraint state for
# the binary path is stale (observed 2026-11-06 07:23 CST). kickstart -k is NOT
# enough; the job needs a full bootout + bootstrap, and the binary must be
# freshly ad-hoc signed and verified before loading.
#
# Usage: bash scripts/tray-reload.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/fusion-tray"
LABEL="com.fusion.tray"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
UID_N="$(id -u)"

[ -f "$BIN" ] || { echo "error: $BIN not found (run: cargo build --release --workspace)" >&2; exit 1; }
[ -f "$PLIST" ] || { echo "error: $PLIST not found (generate from scripts/com.fusion.tray.plist.template)" >&2; exit 1; }

echo "[1/4] re-signing (ad-hoc) $BIN"
codesign --force --sign - "$BIN"

echo "[2/4] verifying signature (strict)"
codesign --verify --strict "$BIN"

echo "[3/4] full reload: bootout + bootstrap $LABEL"
launchctl bootout "gui/${UID_N}/${LABEL}" 2>/dev/null || true
sleep 1
launchctl bootstrap "gui/${UID_N}" "$PLIST"

echo "[4/4] checking process"
sleep 2
if pgrep -fl fusion-tray >/dev/null; then
    pgrep -fl fusion-tray
    echo "tray reloaded OK"
else
    echo "error: tray not running after reload — check /tmp/fusion-tray.log and DiagnosticReports" >&2
    exit 1
fi
