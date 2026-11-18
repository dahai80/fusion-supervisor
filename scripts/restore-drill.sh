#!/usr/bin/env bash
# issue #132: backup restore drill.
# Verifies the full backup chain end to end on a scratch database:
#   encrypted dump -> decrypt -> gunzip -> restore into scratch DB -> row count check.
# Never touches live databases. Safe to run anywhere the backup target is mounted.
#
# Usage:
#   FUSION_BACKUP_TARGET=/opt/fusion-backups \
#   FUSION_BACKUP_ENC_KEY=<key> \
#   DRILL_PG_BIN=/usr/lib/postgresql/16/bin \
#   bash scripts/restore-drill.sh [backup-file.sql.gz.enc]
#
# With no file argument the newest fusion-pg-*.{sql.gz,sql.gz.enc} is used.
# Exit 0 = drill passed; non-zero = chain broken (finds the broken link).
set -euo pipefail

TARGET="${FUSION_BACKUP_TARGET:?FUSION_BACKUP_TARGET not set}"
PG_BIN="${DRILL_PG_BIN:-}"
SCRATCH_DB="${DRILL_SCRATCH_DB:-fusion_restore_drill}"

log() { echo "[restore-drill] $*"; }

# 1. locate newest backup
FILE="${1:-}"
if [ -z "$FILE" ]; then
    FILE=$(ls -1 "$TARGET"/fusion-pg-*.sql.gz "$TARGET"/fusion-pg-*.sql.gz.enc 2>/dev/null | sort | tail -1)
fi
[ -n "$FILE" ] && [ -f "$FILE" ] || { log "FAIL: no backup file found in $TARGET"; exit 1; }
log "backup file: $FILE"

WORK=$(mktemp -d /tmp/fusion-restore-drill.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# 2. decrypt (only when .enc — encryption is fail-closed at backup time)
PLAIN="$WORK/dump.sql.gz"
case "$FILE" in
    *.enc)
        [ -n "${FUSION_BACKUP_ENC_KEY:-}" ] || { log "FAIL: .enc backup but FUSION_BACKUP_ENC_KEY not set"; exit 1; }
        openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
            -in "$FILE" -out "$PLAIN" -pass env:FUSION_BACKUP_ENC_KEY \
            || { log "FAIL: decrypt failed (wrong key or corrupt file)"; exit 1; }
        log "decrypt: OK"
        ;;
    *) cp "$FILE" "$PLAIN"; log "decrypt: skipped (plaintext backup)"; ;;
esac

# 3. gunzip
gunzip -c "$PLAIN" > "$WORK/dump.sql" || { log "FAIL: gunzip failed"; exit 1; }
SIZE=$(wc -c < "$WORK/dump.sql")
[ "$SIZE" -gt 1000 ] || { log "FAIL: decompressed dump suspiciously small ($SIZE bytes)"; exit 1; }
log "gunzip: OK ($SIZE bytes)"

# 4. restore into scratch DB (drop first so the drill is repeatable)
PSQL="${PG_BIN:+$PG_BIN/}psql"
"$PSQL" -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH_DB" >/dev/null
"$PSQL" -d postgres -c "CREATE DATABASE $SCRATCH_DB" >/dev/null
"$PSQL" -d "$SCRATCH_DB" -v ON_ERROR_STOP=1 -q < "$WORK/dump.sql" \
    || { log "FAIL: restore into $SCRATCH_DB errored"; exit 1; }
log "restore into scratch DB '$SCRATCH_DB': OK"

# 5. row count per table as integrity evidence
COUNTS=$("$PSQL" -d "$SCRATCH_DB" -t -A -F, \
    -c "SELECT relname, n_live_tup FROM pg_stat_user_tables ORDER BY relname" 2>/dev/null)
log "table row counts:"
echo "$COUNTS" | sed 's/^/    /'
[ -n "$COUNTS" ] || log "note: dump contains no user tables"

# 6. cleanup scratch DB
"$PSQL" -d postgres -c "DROP DATABASE $SCRATCH_DB" >/dev/null
log "scratch DB dropped"
log "PASS: full chain verified (decrypt -> gunzip -> restore -> counts)"
