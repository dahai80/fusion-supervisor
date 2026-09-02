#!/bin/bash
# Postgres init: create multiple databases in the single shared PG instance.
# Triggered by POSTGRES_MULTIPLE_DATABASES env (comma-separated) at first boot.
# Each fusion app (cowork/identity/rag) connects to its own DB via name — no
# per-app self-hosted Postgres (consolidates the dev-only cowork postgres per
# issue #1). RLS / per-tenant isolation remains each app's responsibility.
set -euo pipefail

databases="${POSTGRES_MULTIPLE_DATABASES:-}"
if [ -z "$databases" ]; then
    echo "[pg-init] POSTGRES_MULTIPLE_DATABASES unset, skipping"
    exit 0
fi

user="${POSTGRES_USER:-fusion}"
for db in $(echo "$databases" | tr ',' ' '); do
    db=$(echo "$db" | xargs)  # trim whitespace
    [ -z "$db" ] && continue
    echo "[pg-init] creating database: $db (owner: $user)"
    psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" <<-EOSQL
        CREATE DATABASE "$db" OWNER "$user";
        GRANT ALL PRIVILEGES ON DATABASE "$db" TO "$user";
EOSQL
done
echo "[pg-init] done: $(echo "$databases" | tr ',' ' ')"
