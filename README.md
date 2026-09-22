# World Loom Server

An experimental Rust server where players and AI agents build in the same
persistent voxel world. [Valence](https://github.com/valence-rs/valence) provides
the Minecraft 1.20.1 protocol layer; this project owns world rules, SQLite/Anvil
persistence, an embedded browser bridge, and MCP tools for inspected, validated
and undoable edits.

- Server-authoritative creative world shared by browser clients and agents.
- Bounded edits validated through one command path.
- Build plans with idempotent application and persistent undo records.
- SQLite metadata and Anvil region storage survive restarts.
- Optional MCP bearer authentication and explicit browser-origin checks.

This is a prototype, not a complete Minecraft implementation or a public-server
hosting platform. Game connections use offline identities, not authenticated
accounts. Keep game and browser endpoints on loopback or a trusted private
network. MCP authentication does not authenticate game players.

Known Valence dependency advisories remain; read [SECURITY.md](SECURITY.md#known-dependency-advisories-2026-09-22)
before deployment.

## Try it without accounts or private infrastructure

Requires Rust 1.92 and Python 3.10+ on Linux or macOS. No browser, model API,
Tailscale account or private coordination repository is needed for this demo.

```sh
git clone https://github.com/minifish-org/world-loom-server.git
cd world-loom-server
python3 scripts/demo.py
```

The demo builds the server, starts an isolated temporary world on loopback,
applies a block build through MCP, retries it idempotently, restarts the server,
and undoes the persisted build. It stops its own server and deletes only its
temporary data when finished.

For an interactive world, use separate terminals:

```sh
# Terminal 1: keep both listeners local.
WORLD_LOOM_GAME_ADDR=127.0.0.1:25565 \
WORLD_LOOM_BRIDGE_ADDR=127.0.0.1:18081 cargo run --locked
```

```sh
# Terminal 2: public browser client, pinned compatible revision.
git clone https://github.com/minifish-org/world-loom-client.git
cd world-loom-client
git checkout a0ea7764049e6f58d7859e92a1ea2f44f0414470
# Install Node.js 22 and pnpm 10.32.1 first.
pnpm install --frozen-lockfile
pnpm start:world-loom
```

Open `http://localhost:3000`, use server `localhost:25565`, protocol `1.20.1`,
and proxy `:18081`. Use distinct usernames to try two browser clients.
The client has its own license and asset/dependency requirements; see its README.
A Minecraft Java 1.20.1-compatible desktop client can also join the local server.

The MCP URL is `http://127.0.0.1:18081/mcp`. See [MCP tools](#mcp-tools) below.
Historical implementation details are in [development milestones](docs/milestones.md).

## Run

```sh
cargo run
```

The server listens on Valence's default `0.0.0.0:25565`.

### Docker

Build the production image for the homelab target:

```sh
docker build --platform linux/amd64 -t world-loom-server:local .
```

The container stores all durable state under `/var/lib/world-loom`. Mount that
directory and publish only the browser bridge to host loopback:

```sh
docker run --rm \
  -p 127.0.0.1:18082:18081 \
  -v "$PWD/data:/var/lib/world-loom" \
  -v "$PWD/config/world-loom-mcp-api-key:/run/secrets/world-loom-mcp-api-key:ro" \
  -e WORLD_LOOM_ALLOWED_ORIGINS=https://world-loom-client.pages.dev \
  -e WORLD_LOOM_HEALTH_ORIGIN=https://world-loom-client.pages.dev \
  -e WORLD_LOOM_MCP_API_KEY_FILE=/run/secrets/world-loom-mcp-api-key \
  world-loom-server:local
```

The game listener on `25565` remains inside the container because the embedded
browser bridge connects to it over container loopback. Production releases are
published manually to GHCR by `.github/workflows/container.yml`; deployments
must pin the resulting image digest instead of following a mutable tag.

By default, V3.1 stores SQLite metadata at:

```text
data/world-loom.sqlite3
```

and Anvil region files at:

```text
data/anvil/region
```

Use `WORLD_LOOM_DB_PATH` and `WORLD_LOOM_REGION_DIR` to override these paths for local tests:

```sh
WORLD_LOOM_DB_PATH=/tmp/world-loom-test.sqlite3 \
WORLD_LOOM_REGION_DIR=/tmp/world-loom-test-anvil-region \
  cargo run
```

The MCP endpoint shares the browser bridge listener and defaults to:

```text
http://127.0.0.1:18081/mcp
```

Local development leaves MCP authentication disabled unless
`WORLD_LOOM_MCP_API_KEY_FILE` points to a readable, non-empty token file. When
configured, MCP GET and POST requests must send the token as a Bearer
credential:

```text
Authorization: Bearer <token>
```

The token is hashed when the bridge starts and is never logged. Browser bridge
routes under `/api/vm/net/` and their health checks do not require this token.

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

The deployment path keeps MCP on the bridge listener at `127.0.0.1:18081/mcp` by default.

For a host that already owns Tailscale HTTPS `:443`, mount the browser prefix
and the authenticated MCP endpoint separately:

```sh
tailscale serve --https=443 --set-path=/api/vm/net/ --bg \
  http://127.0.0.1:18082/api/vm/net/
tailscale serve --https=443 --set-path=/mcp --bg \
  http://127.0.0.1:18082/mcp
```

Codex can connect directly to the tailnet-only HTTPS `/mcp` URL with an
`Authorization: Bearer ...` header. Do not enable Tailscale Funnel for this
listener.

## Checks

```sh
cargo fmt
cargo clippy
cargo test
```

Run the standalone integration demo against a temporary world:

```sh
python3 scripts/demo.py
```

Browser multiplayer checks are a separate manual step using the public client
quick start above; the demo verifies MCP, persistence and undo without a browser.

## MCP Tools

Read-only:

- `server_status`: tick, player count, bounds, database path, Anvil region path, MCP endpoint, MSPT telemetry, chunk interest/lifecycle stats, bridge backpressure config, SQLite schema/save format versions, dirty chunk count, and writer queue stats.
- `list_players`: connected player names, positions, and ping.
- `get_world_bounds`: bounded world coordinates.
- `list_build_palette`: read-only discovery for the versioned 115-block
  building palette and representative color metadata.
- `get_block`: one live block.
- `snapshot_region`: live block snapshot with max volume `512`.

Edit:

- `set_block`: one block from Build Palette v1; call `list_build_palette` for
  canonical names.
- `remove_block`: one block removal.
- `fill_region`: bounded cuboid edit with max volume `128`; accepts the same
  build palette plus `air` for removal.

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

`block_overrides` remains in the schema for legacy V1/V2 SQLite delta fallback. V3.1 chunk bulk writes go to Anvil `.mca` region files instead of adding new `block_overrides` rows.

## Reset / Backup

Back up the local save with the V3.1 backup script:

```sh
./scripts/backup-save.sh
```

By default this writes an ignored `backups/world-loom-*.sqlite3` file, a copied `backups/world-loom-*-anvil-region` directory, and a JSON manifest. Set `WORLD_LOOM_DB_PATH`, `WORLD_LOOM_REGION_DIR`, and `WORLD_LOOM_BACKUP_DIR` to override the source and destination.

Clear the default local save:

```sh
./scripts/clear-save.sh
```

The clear script deletes the DB, SQLite `-wal`/`-shm` sidecars, and Anvil region directory. It refuses paths outside this repo's `data/` directory or `/tmp/world-loom-*`.

## Not In V3.1

- No multi-user or per-tool MCP permission system.
- No public Cloudflare Worker proxy.
- No client UI rewrite.
- No new gameplay systems.
- No true infinite world yet.
- No Valence core changes.
- No broad Minecraft world import/export UX yet.

## Next Direction

After V3.1, the next practical work is V4 family-play hardening: production process management, MCP authorization/audit UX, simple admin backup/restore operations, and the first World Loom-specific visible behavior.

## License

AGPL-3.0-or-later; see [LICENSE](LICENSE). Dependencies and the separately
distributed browser client retain their own licenses. No Minecraft game assets
or private world saves are included in this server repository.
