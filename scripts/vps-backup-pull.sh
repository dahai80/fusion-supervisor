#!/usr/bin/env bash
# issue #132 off-host copy: pull VPS backups + encryption key to this Mac.
# Pull-based (VPS holds no credentials here); mirror is self-sufficient for
# restore (key rides along, 0600). Scheduled by launchd
# com.fusion.vps-backup-pull.plist. Log tail is the health signal.
set -u

MIRROR="$HOME/.fusion-backups-vps"
LOG="$MIRROR/pull.log"
mkdir -p "$MIRROR"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG"; }

# 1. backups (mirror = exact copy)
if ! rsync -a --delete fusion-vps:/opt/fusion-backups/ "$MIRROR/backups/" >> "$LOG" 2>&1; then
    log "FAIL: rsync backups"
    exit 1
fi
log "backups synced: $(ls "$MIRROR/backups" 2>/dev/null | wc -l | tr -d ' ') files"

# 2. encryption key copy (0600; value never printed)
if ssh fusion-vps 'sudo -n test -f /opt/fusion-sv/env' 2>/dev/null; then
    if ssh fusion-vps 'sudo -n cat /opt/fusion-sv/env' > "$MIRROR/env" 2>> "$LOG" \
        && chmod 600 "$MIRROR/env"; then
        log "env (target + enc key) synced"
    else
        log "FAIL: env sync"
        exit 1
    fi
else
    log "WARN: /opt/fusion-sv/env absent on VPS (key not yet provisioned?)"
fi
log "pull ok"
