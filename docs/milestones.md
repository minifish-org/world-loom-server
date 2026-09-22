# Development milestones

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

- MCP endpoint at `/mcp`, served by the Rust bridge HTTP listener.
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
- Successful edits still route through `WorldCommand`, then enqueue persistence work.
- Backup and clear-save scripts cover both SQLite metadata and chunk bulk files.

## What V3.1 Corrects

- Production chunk bulk storage is now Anvil `.mca` region files through `AnvilChunkStorage`.
- `HybridAnvilStorage` keeps SQLite for metadata, schema/save format version, command log, dirty chunk index, backup manifest, and legacy SQLite delta fallback.
- The old V3 JSON region backend is not the default production path.
- Unedited chunks are still generated from the deterministic superflat generator and are not written to disk.
- Dirty chunks are written as Anvil region chunks; reverting a dirty chunk to generated base removes the Anvil chunk.
- SQLite `storage_backend` is `anvil_chunk_sqlite_metadata`.
- SQLite schema version is `4`; save format version is `3`.

