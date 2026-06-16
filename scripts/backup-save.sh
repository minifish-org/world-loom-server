#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DB_PATH=${WORLD_LOOM_DB_PATH:-"$ROOT/data/world-loom.sqlite3"}
REGION_DIR=${WORLD_LOOM_REGION_DIR:-"$ROOT/data/regions"}
BACKUP_DIR=${WORLD_LOOM_BACKUP_DIR:-"$ROOT/backups"}

if [ ! -f "$DB_PATH" ]; then
  echo "No World Loom SQLite save found at $DB_PATH" >&2
  exit 1
fi

mkdir -p "$BACKUP_DIR"

STAMP=$(date -u +"%Y%m%dT%H%M%SZ")
BACKUP_PATH="$BACKUP_DIR/world-loom-$STAMP.sqlite3"
REGION_BACKUP_PATH="$BACKUP_DIR/world-loom-$STAMP-regions"
MANIFEST_PATH="$BACKUP_DIR/world-loom-$STAMP.manifest.json"

if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$DB_PATH" ".backup '$BACKUP_PATH'"
  INTEGRITY=$(sqlite3 "$BACKUP_PATH" "PRAGMA integrity_check;")
  SCHEMA_VERSION=$(sqlite3 "$BACKUP_PATH" "SELECT schema_version FROM world_metadata WHERE world_id = 'default';" 2>/dev/null || echo "unknown")
  SAVE_FORMAT_VERSION=$(sqlite3 "$BACKUP_PATH" "SELECT save_format_version FROM world_metadata WHERE world_id = 'default';" 2>/dev/null || echo "unknown")
  STORAGE_BACKEND=$(sqlite3 "$BACKUP_PATH" "SELECT storage_backend FROM world_metadata WHERE world_id = 'default';" 2>/dev/null || echo "unknown")
else
  if [ -f "$DB_PATH-wal" ] || [ -f "$DB_PATH-shm" ]; then
    echo "sqlite3 is required for a reliable online backup while WAL sidecars exist." >&2
    exit 1
  fi
  cp "$DB_PATH" "$BACKUP_PATH"
  INTEGRITY="not_checked"
  SCHEMA_VERSION="unknown"
  SAVE_FORMAT_VERSION="unknown"
  STORAGE_BACKEND="unknown"
fi

rm -rf "$REGION_BACKUP_PATH"
if [ -d "$REGION_DIR" ]; then
  mkdir -p "$REGION_BACKUP_PATH"
  cp -R "$REGION_DIR"/. "$REGION_BACKUP_PATH"/
else
  mkdir -p "$REGION_BACKUP_PATH"
fi

cat >"$MANIFEST_PATH" <<EOF
{
  "created_at_utc": "$STAMP",
  "source_path": "$DB_PATH",
  "backup_path": "$BACKUP_PATH",
  "region_source_path": "$REGION_DIR",
  "region_backup_path": "$REGION_BACKUP_PATH",
  "storage_backend": "$STORAGE_BACKEND",
  "schema_version": "$SCHEMA_VERSION",
  "save_format_version": "$SAVE_FORMAT_VERSION",
  "integrity_check": "$INTEGRITY"
}
EOF

echo "World Loom backup written:"
echo "  database: $BACKUP_PATH"
echo "  regions: $REGION_BACKUP_PATH"
echo "  manifest: $MANIFEST_PATH"
