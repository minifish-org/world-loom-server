#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DB_PATH=${WORLD_LOOM_DB_PATH:-"$ROOT/data/world-loom.sqlite3"}

case "$DB_PATH" in
  "$ROOT"/data/*|/tmp/world-loom-*)
    ;;
  *)
    echo "Refusing to delete save outside $ROOT/data or /tmp/world-loom-*:" >&2
    echo "  $DB_PATH" >&2
    exit 1
    ;;
esac

rm -f "$DB_PATH" "$DB_PATH-shm" "$DB_PATH-wal"
echo "Cleared World Loom save: $DB_PATH"
