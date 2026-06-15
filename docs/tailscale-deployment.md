# World Loom Tailscale Deployment

V1.2 target path:

```text
Cloudflare Pages static client
  -> HTTPS/WSS on <machine>.<tailnet>.ts.net
  -> Caddy reverse proxy
  -> world-loom-server Rust bridge on 127.0.0.1:18081
  -> Valence game server on localhost:25565
```

Cloudflare Pages serves static files only. Do not add a Cloudflare Worker proxy for the browser bridge in V1.2.

## Ports

```text
25565  Valence Minecraft TCP server
18081  Rust browser bridge HTTP/WebSocket endpoint
8765   local MCP HTTP endpoint
443    Caddy HTTPS/WSS reverse proxy on the Tailscale host
```

Recommended binding:

- `WORLD_LOOM_BRIDGE_ADDR=127.0.0.1:18081`
- `WORLD_LOOM_MCP_ADDR=127.0.0.1:8765`
- Caddy exposes only the browser bridge over HTTPS to tailnet users.
- Keep MCP local unless a later milestone adds auth/permissions.

## Tailscale Prerequisites

In the Tailscale admin console:

1. Enable MagicDNS.
2. Enable HTTPS certificates.
3. Confirm the server machine has a stable machine name.
4. Confirm family devices are members of the tailnet and can resolve `<machine>.<tailnet>.ts.net`.

Do not commit the real tailnet hostname to git.

## Server Environment

Use a persistent data directory outside the repo:

```sh
sudo mkdir -p /var/lib/world-loom
sudo chown "$USER" /var/lib/world-loom
```

Run the server:

```sh
WORLD_LOOM_DB_PATH=/var/lib/world-loom/world-loom.sqlite3 \
WORLD_LOOM_BRIDGE_ADDR=127.0.0.1:18081 \
WORLD_LOOM_ALLOWED_ORIGINS=https://<project>.pages.dev,https://<custom-domain> \
WORLD_LOOM_MCP_ADDR=127.0.0.1:8765 \
cargo run --release
```

For local-only testing without Pages, keep:

```sh
WORLD_LOOM_ALLOWED_ORIGINS=http://localhost:3000,http://127.0.0.1:3000
```

## HTTPS/WSS With Caddy

Generate or renew a Tailscale certificate on the server machine:

```sh
sudo mkdir -p /var/lib/world-loom/tls
sudo tailscale cert \
  --cert-file /var/lib/world-loom/tls/<machine>.<tailnet>.ts.net.crt \
  --key-file /var/lib/world-loom/tls/<machine>.<tailnet>.ts.net.key \
  <machine>.<tailnet>.ts.net
```

Example Caddyfile:

```caddyfile
<machine>.<tailnet>.ts.net {
	bind <tailscale-ip>
	tls /var/lib/world-loom/tls/<machine>.<tailnet>.ts.net.crt /var/lib/world-loom/tls/<machine>.<tailnet>.ts.net.key

	reverse_proxy 127.0.0.1:18081
}
```

Caddy supports WebSocket upgrade traffic through `reverse_proxy`, so the same site handles:

```text
https://<machine>.<tailnet>.ts.net/api/vm/net/connect
wss://<machine>.<tailnet>.ts.net/api/vm/net/socket
```

If the host has public network interfaces, bind Caddy to the Tailscale IP rather than all interfaces. The Rust bridge should stay on `127.0.0.1:18081` behind Caddy.

## Optional Tailscale Serve Check

For quick debugging, Tailscale Serve can reverse proxy local services inside the tailnet. Use it only to check basic reachability; keep Caddy as the recommended V1.2 runbook because it is explicit, inspectable, and easy to pair with the existing Rust bridge.

## Client Configuration

Set the Cloudflare Pages build variables in `world-loom-client`:

```text
WORLD_LOOM_CLIENT_SERVER=localhost:25565
WORLD_LOOM_CLIENT_PROXY=https://<machine>.<tailnet>.ts.net
```

Set server allowed origins to match the Pages browser origin:

```text
WORLD_LOOM_ALLOWED_ORIGINS=https://<project>.pages.dev
```

Add a custom domain to both the Pages env var and `WORLD_LOOM_ALLOWED_ORIGINS` if used.

## Backup

Stop the server before a simple file copy:

```sh
cp /var/lib/world-loom/world-loom.sqlite3 /var/backups/world-loom-$(date +%Y%m%d-%H%M%S).sqlite3
```

If the server must remain running, use SQLite's online backup command:

```sh
sqlite3 /var/lib/world-loom/world-loom.sqlite3 \
  ".backup '/var/backups/world-loom-$(date +%Y%m%d-%H%M%S).sqlite3'"
```

Back up `-wal` and `-shm` sidecars only when copying a live database directly. Prefer stopping the server or using SQLite `.backup`.

## Clear Save

The repo safety script only clears repo-local or `/tmp/world-loom-*` paths. For production, stop the server and remove the configured DB explicitly:

```sh
rm -f /var/lib/world-loom/world-loom.sqlite3 \
      /var/lib/world-loom/world-loom.sqlite3-wal \
      /var/lib/world-loom/world-loom.sqlite3-shm
```

Restarting the server recreates an empty generated superflat world.

## Manual Acceptance

1. Start `world-loom-server` with the deployment environment above.
2. Start or reload Caddy.
3. From a tailnet client, open `https://<machine>.<tailnet>.ts.net/api/vm/net/connect` and confirm JSON status.
4. Open the Pages URL.
5. Confirm the browser logs show the HTTPS proxy URL.
6. Join with two browser clients, place and remove blocks, then restart the server and confirm edits persist.

## Not V1.2

- No public Cloudflare Worker TCP/WebSocket proxy.
- No MCP exposure beyond local host.
- No new gameplay.
- No V2 hybrid storage.
- No Valence core fork.
