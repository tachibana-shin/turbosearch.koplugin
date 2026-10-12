#!/bin/bash
# Regenerates the compile-time database that sqlx's checked macros verify
# against, from migrations/0001_init.sql.
#
# Without this, `cargo build` fails with "no DATABASE_URL" the moment a checked
# macro is added, and the fix is to run this script rather than to edit code.
set -euo pipefail
cd "$(dirname "$0")/.."
DB=.sqlx-dev.db
rm -f "$DB"
sqlite3 "$DB" < migrations/0001_init.sql
echo "created $DB from migrations/0001_init.sql"
