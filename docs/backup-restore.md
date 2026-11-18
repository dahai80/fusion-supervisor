---
doc_id: fusion-supervisor/backup-restore
product: fusion-supervisor
class: core
entry: service
doc_type: guide
title: { zh: "备份与恢复", en: "Backup & Restore" }
audience: engineer
status: draft
version: 0.1.65
updated: 2026-11-18
owner: "@dahai"
source_of_truth:
  - src/backup.rs
  - src/config.rs
  - scripts/restore-drill.sh
---

# Backup & Restore (issue #132)

Automated PostgreSQL backup with two-tier retention and at-rest encryption, run by the `fusion-sv` daemon. Qdrant snapshots ride along when the compose plane is enabled.

## How it works

- The daemon tick loop checks `backup_schedule_sec` (default 86400 = daily). When due, `fusion-sv backup` runs in a blocking task (never under the core lock).
- Output: `pg_dumpall` piped through gzip, then encrypted with `openssl enc -aes-256-cbc -pbkdf2 -iter 200000` when `FUSION_BACKUP_ENC_KEY` is set.
- Files land in `FUSION_BACKUP_TARGET`: `fusion-pg-<stamp>.sql.gz.enc` (or `.sql.gz` when no key) plus `fusion-qdrant-<stamp>/` snapshots.
- `<stamp>` is `d<dayIndex>-<hhmmss>` UTC — same-day backups never overwrite each other.
- Partial writes are safe: the dump is written to a `*.tmp` name and renamed only after success. A killed daemon leaves no fake backup behind (prune ignores `.tmp`).

## Configuration (`~/.fusion-sv/config.toml` or `FUSION_SV_CONFIG` target)

| Field | Default | Meaning |
|---|---|---|
| `backup_schedule_sec` | 86400 | How often the daemon triggers a backup |
| `backup_retention` | 7 | Daily backups to keep (newest N by date) |
| `backup_weekly_retention` | 0 | Beyond the daily tier, keep the newest backup of each week, up to N weeks. `0` = flat behavior (issue #132) |
| `backup_retention` = 0 | — | Disables pruning entirely |

Environment:

- `FUSION_BACKUP_TARGET` — backup output directory (required; taken from env or the daemon's `.env` file). Missing target disables the scheduled backup after a one-time WARN; a manual `fusion-sv backup` fails loudly.
- `FUSION_BACKUP_ENC_KEY` — when set, backups are encrypted at rest. The key is passed to the `openssl` child via a private environment variable: it never appears in argv, the parent process environment, logs, or on disk. **Losing this key means losing every encrypted backup.** Store it outside the backup host.
- `FUSION_BACKUP_DATE` — overrides the date stamp (testing only).

## Retention policy example

`backup_retention = 14`, `backup_weekly_retention = 4`: the newest 14 backups are always kept; older candidates are reduced to one per calendar week (the newest of each week), up to 4 weeks. Gives ~1 month of lookback at ~18 files.

## Restore drill

`scripts/restore-drill.sh` verifies the full chain — decrypt, gunzip, restore into a scratch database, table row counts — without touching live databases:

```bash
FUSION_BACKUP_TARGET=/opt/fusion-backups \
FUSION_BACKUP_ENC_KEY=<key> \
DRILL_PG_BIN=/usr/lib/postgresql/16/bin \
bash scripts/restore-drill.sh
```

Run it after the first scheduled backup, then periodically (e.g. monthly). A backup you have never restored is not a backup.

### Manual restore

```bash
# 1. decrypt (skip if the file ends in .sql.gz)
openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
  -in fusion-pg-<stamp>.sql.gz.enc -out dump.sql.gz \
  -pass env:FUSION_BACKUP_ENC_KEY

# 2. restore
gunzip -c dump.sql.gz | psql -d postgres
```

`pg_dumpall` output restores roles and all databases; per-database re-creation is included.

## Off-host copy (recommended)

Backups on the same host as the databases do not survive host loss. Pull from a second machine over SSH:

```bash
rsync -av --delete fusion-vps:/opt/fusion-backups/ /backup/vps-fusion/
```

Schedule it (cron/launchd) on the second machine. Keep `FUSION_BACKUP_ENC_KEY` there too so pulled `.enc` files remain verifiable/restorable.

## VPS deployment notes (fusion-vps)

1. Install the release artifact (`fusion-sv-linux-x86_64.tar.gz` from the GitHub Release) into `/opt/fusion-sv/bin/fusion-sv`, then `systemctl restart fusion-sv`.
2. Create the backup dir and key file:
   ```bash
   sudo mkdir -p /opt/fusion-backups
   echo "FUSION_BACKUP_TARGET=/opt/fusion-backups" | sudo tee -a /opt/fusion-sv/env  # or systemd EnvironmentFile
   ```
   Generate the key once, store it 0600, and also keep a copy off-host:
   ```bash
   openssl rand -base64 32   # becomes FUSION_BACKUP_ENC_KEY
   ```
3. `pg_dumpall` needs a superuser or a role with read access to all databases. Verify `sudo -u postgres /usr/bin/pg_dumpall --list-only` works under the `ops` user, and set the connection env accordingly.
4. Trigger one manual backup (`curl` the daemon HTTP face or `fusion-sv backup`), then run the restore drill.
