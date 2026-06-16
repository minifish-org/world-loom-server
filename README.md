# World Loom Server

This repository owns the future Rust authoritative server for World Loom Online.

M2 contains a minimal Valence-backed authoritative server prototype. M3 connects the browser client. M4 verifies two browser clients sharing one server-backed world, including block place/remove replication, disconnect/reconnect, and basic telemetry. M5 persists block edits and restores them after server restart. M6 adds local MCP inspect/edit tools over the live world. V1.1 moves the browser WebSocket-to-TCP bridge into this Rust server process. V2 hardens multiplayer performance visibility and SQLite save reliability without adding gameplay. V3 adds chunk lifecycle and region-file chunk storage foundations. The server remains intentionally small: no complex permission system, no new gameplay, and no Valence core fork.

## What M2 Provides

- Valence server using Minecraft protocol 1.20.1.
- Offline auth mode for local development.
- Bounded superflat world centered at `0,0`.
- Players spawn at the center in creative mode.
- Hotbar slots contain basic building blocks.
- Player block placement and removal route through `WorldCommand`.
- `WorldCommand` validates bounds, allowed block types, empty placement targets, non-air anchors, spawn-space safety, and protected foundation blocks.

## What M4 Adds

- Multiple browser clients can join the same in-memory server world.
- Block place/remove mutations still route through `WorldCommand` and replicate through Valence world state.
- Disconnect/reconnect keeps the in-memory world state valid while the server process remains running.
- Debug telemetry reports server tick timing, connected player count, and per-player ping status through logs plus tab list/action bar messages.
- Client FPS remains a browser-client concern and is shown through the existing client debug overlay when renderer FPS is available.

## What M5 Adds

- SQLite persistence through `rusqlite`.
- Successful `WorldCommand` mutations enqueue block edits to a background writer.
- The writer batches updates and keeps disk I/O off the gameplay event path.
- Server startup loads saved `block_overrides` after generating the bounded base world.
- Reverting a cell back to its generated base block removes the override.
- Removing a generated base-world block persists an explicit `air` override.
- A command log records successful `set_block` and `remove_block` operations.

## What M6 Adds

- Local MCP endpoint at `http://127.0.0.1:8765/mcp`.
- Streamable HTTP-style JSON-RPC support for `initialize`, `ping`, `tools/list`, and `tools/call`.
- Read-only tools: `server_status`, `list_players`, `get_world_bounds`, `get_block`, `snapshot_region`.
- Edit tools: `set_block`, `remove_block`, `fill_region`.
- MCP HTTP handlers enqueue requests only; live world reads/writes happen on the server update loop.
- All MCP edit tools route through `WorldCommand` validation.
- Successful MCP edits enqueue SQLite persistence and replicate to connected browser clients.

## What V1.1 Adds

- Rust-owned browser bridge endpoint compatible with the existing `minecraft-web-client` proxy protocol.
- Default bridge address: `http://127.0.0.1:18081/api/vm/net`.
- Valence TCP remains on `0.0.0.0:25565`.
- Browser traffic no longer requires the client-owned Node `server.js` bridge on the target path.

## What V1.2 Documents

- Tailscale private-host deployment.
- Caddy HTTPS/WSS reverse proxy in front of the local Rust bridge.
- Pages/client environment variables and required allowed origins.
- Persistent SQLite data directory, backup, and clear-save workflow.

## What V2 Adds

- Server tick/MSPT telemetry in logs, tab list/action bar, and MCP `server_status`.
- Per-player ping visibility remains in tab/action bar, player list, and MCP status.
- Valence `ViewDistance` based chunk interest management with a bounded server default.
- Rust browser bridge backpressure knobs: TCP read buffer size, WebSocket queue capacity, and pending connection limit.
- SQLite save format versioning with V1 metadata migration.
- `WorldStorage` trait plus the current `SqliteDeltaStorage` implementation.
- SQLite writer queue stats in MCP `server_status`.
- Online-safe backup script with a JSON manifest.

## What V3 Adds

- Bounded world expands to 512x512 horizontal blocks, centered at `0,0`.
- The server no longer generates or inserts every chunk at startup.
- Chunks load/generate around player interest and unload when no longer needed.
- MCP reads/edits load required chunks before accessing live world state.
- `WorldStorage` now exposes chunk-oriented operations.
- `RegionChunkStorage` stores edited chunk bulk data under `data/regions` by default.
- `HybridRegionStorage` keeps SQLite for metadata, command log, dirty chunk index, backup manifest, and legacy SQLite delta fallback.
- Successful edits still route through `WorldCommand`, then enqueue persistence work.
- Backup and clear-save scripts cover both SQLite metadata and region chunk files.

## Run

```sh
cargo run
```

The server listens on Valence's default `0.0.0.0:25565`.

By default, V3 stores SQLite metadata at:

```text
data/world-loom.sqlite3
```

and region chunk files at:

```text
data/regions
```

Use `WORLD_LOOM_DB_PATH` and `WORLD_LOOM_REGION_DIR` to override these paths for local tests:

```sh
WORLD_LOOM_DB_PATH=/tmp/world-loom-test.sqlite3 \
WORLD_LOOM_REGION_DIR=/tmp/world-loom-test-regions \
  cargo run
```

The MCP endpoint defaults to:

```text
http://127.0.0.1:8765/mcp
```

Override it with:

```sh
WORLD_LOOM_MCP_ADDR=127.0.0.1:9876 cargo run
```

The browser bridge endpoint defaults to:

```text
http://127.0.0.1:18081/api/vm/net
```

Override it with:

```sh
WORLD_LOOM_BRIDGE_ADDR=127.0.0.1:18082 cargo run
```

Allowed browser origins default to local Rsbuild dev origins:

```text
http://localhost:3000,http://127.0.0.1:3000
```

Override them with:

```sh
WORLD_LOOM_ALLOWED_ORIGINS=http://localhost:3000 cargo run
```

V2 networking and interest defaults can also be tuned:

```sh
WORLD_LOOM_VIEW_DISTANCE_CHUNKS=6 cargo run
WORLD_LOOM_BRIDGE_TCP_READ_BUFFER_BYTES=16384 cargo run
WORLD_LOOM_BRIDGE_WS_QUEUE_CAPACITY=1024 cargo run
WORLD_LOOM_BRIDGE_MAX_PENDING_CONNECTIONS=128 cargo run
```

`WORLD_LOOM_VIEW_DISTANCE_CHUNKS` is clamped to `2..=12` by this server. Valence still owns the actual chunk synchronization.

## Connect

Use a Minecraft Java 1.20.1-compatible client:

```text
Server address: localhost:25565
Authentication: offline/local development
```

For another machine on the same private network, use the host machine's LAN or Tailscale address with port `25565`.

The browser client path uses the Rust-owned bridge:

1. Start this server with `cargo run`.
2. In `world-loom-client`, run `pnpm start:world-loom`.
3. Open the browser client and connect to `localhost:25565` with version `1.20.1`.
4. Use local proxy `:18081`, which points at this server's browser bridge.
5. Use distinct usernames for multiple browser clients.

The old Node bridge in `world-loom-client/server.js` is retained only as a fallback/debug tool, normally on `:18080`.

## Tailscale Deployment

Use `docs/tailscale-deployment.md` for the family-play deployment runbook:

```text
Cloudflare Pages static client
  -> Tailscale HTTPS/WSS host
  -> Caddy reverse proxy
  -> Rust browser bridge on 127.0.0.1:18081
  -> Valence TCP server on localhost:25565
```

The deployment path keeps MCP on `127.0.0.1:8765` by default.

## Checks

```sh
cargo fmt
cargo clippy
cargo test
```

The stack-level smoke tests start the local repos and run browser verification:

```sh
../scripts/smoke-m4-dual-client.sh
../scripts/smoke-m5-persistence.sh
../scripts/smoke-m6-mcp.sh
../scripts/smoke-v3-storage.sh
```

## MCP Tools

Read-only:

- `server_status`: tick, player count, bounds, database path, region path, MCP endpoint, MSPT telemetry, chunk interest/lifecycle stats, bridge backpressure config, SQLite schema/save format versions, dirty chunk count, and writer queue stats.
- `list_players`: connected player names, positions, and ping.
- `get_world_bounds`: bounded world coordinates.
- `get_block`: one live block.
- `snapshot_region`: live block snapshot with max volume `512`.

Edit:

- `set_block`: one block; supported blocks are `stone`, `dirt`, `grass_block`, `oak_planks`, `cobblestone`, and `glass`.
- `remove_block`: one block removal.
- `fill_region`: bounded cuboid edit with max volume `128`; accepts the same set blocks plus `air` for removal.

All edit tools use `WorldCommand`, so bounds, spawn protection, foundation protection, supported block checks, target occupancy, and placement anchor rules still apply.

## SQLite Schema

`world_metadata`

- `world_id`
- `storage_backend`
- `schema_version`
- `save_format_version`
- bounded world min/max coordinates
- timestamps

`schema_migrations`

- applied schema version rows
- migration description
- timestamp

`block_overrides`

- `world_id`
- `x`, `y`, `z`
- `block_state_raw`
- `updated_at`

`command_log`

- append-only successful command rows
- `command_kind`
- `x`, `y`, `z`
- final `block_state_raw`
- `recorded_at`

`dirty_chunks`

- dirty chunk index keyed by chunk x/z
- dirty count and last dirty/flush timestamps

`backup_manifest`

- SQLite source/backup paths
- region source/backup paths
- schema/save format versions
- creation timestamp

`block_state_raw` is Valence `BlockState::to_raw()` for the current Minecraft protocol version.

`block_overrides` remains in the schema for legacy V1/V2 SQLite delta fallback. V3 chunk bulk writes go to region files instead of adding new `block_overrides` rows.

## Reset / Backup

Back up the local save with the V3 backup script:

```sh
./scripts/backup-save.sh
```

By default this writes an ignored `backups/world-loom-*.sqlite3` file, a copied `backups/world-loom-*-regions` directory, and a JSON manifest. Set `WORLD_LOOM_DB_PATH`, `WORLD_LOOM_REGION_DIR`, and `WORLD_LOOM_BACKUP_DIR` to override the source and destination.

Clear the default local save:

```sh
./scripts/clear-save.sh
```

The clear script deletes the DB, SQLite `-wal`/`-shm` sidecars, and region directory. It refuses paths outside this repo's `data/` directory or `/tmp/world-loom-*`.

## Not In V3

- No complex MCP permission system.
- No public Cloudflare Worker proxy.
- No client UI rewrite.
- No new gameplay systems.
- No true infinite world yet.
- No Valence core changes.
- No direct Minecraft Anvil import/export yet.

## Next Direction

After V3, the next practical work is V4 family-play hardening: production process management, MCP authorization/audit UX, simple admin backup/restore operations, and the first World Loom-specific visible behavior.
