#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DB_PATH=${WORLD_LOOM_DB_PATH:-"$ROOT/data/world-loom.sqlite3"}
REGION_DIR=${WORLD_LOOM_REGION_DIR:-"$ROOT/data/regions"}

case "$DB_PATH" in
  "$ROOT"/data/*|/tmp/world-loom-*)
    ;;
  *)
    echo "Refusing to delete save outside $ROOT/data or /tmp/world-loom-*:" >&2
    echo "  $DB_PATH" >&2
    exit 1
    ;;
esac

case "$REGION_DIR" in
  "$ROOT"/data/*|/tmp/world-loom-*)
    ;;
  *)
    echo "Refusing to delete region storage outside $ROOT/data or /tmp/world-loom-*:" >&2
    echo "  $REGION_DIR" >&2
    exit 1
    ;;
esac

rm -f "$DB_PATH" "$DB_PATH-shm" "$DB_PATH-wal"
rm -rf "$REGION_DIR"
echo "Cleared World Loom save: $DB_PATH"
echo "Cleared World Loom regions: $REGION_DIR"
