# Built-in Tailscale

Astesia can reach a database through its own embedded Tailscale node instead of
the machine's network. The node joins the tailnet as a separate machine. The embedded node signs in separately from the Tailscale
client installed on the machine. For example, the system client can stay on a
personal tailnet while Astesia reaches databases on a work tailnet.

## Using it

1. Edit a network connection and turn on **Connect through built-in Tailscale**.
2. Enter the database's tailnet address as the host, either a MagicDNS name
   such as `db.tail1234.ts.net` or a `100.x` address.
3. Connect. The first time, Astesia opens the Tailscale sign-in page in the
   browser and waits up to five minutes for the sign-in to finish.


The node accepts subnet routes, so a database behind a subnet router (for
example a private `10.x` address in a company VPC) is reachable too. When a
connection fails, the error names the tailnet's reason, such as no route to
the host.

If the tailnet requires device approval, approve the device in the Tailscale
admin console and connect again.

## Settings → Tailscale

Open it from the gear in the status bar, or with ⌘, / Ctrl+,.

- **Sign In… / Sign Out…** shows the signed-in account and switches accounts.
- **Connect on Launch** starts the node when Astesia opens, so the device stays
  online on the tailnet even before a Tailscale connection is used.
- **Tailscale Address** shows the device's MagicDNS name and tailnet address.
- **Tags** lists the tags a tailnet admin set on the device. Tags are managed in
  the Tailscale admin console.
- **Preferred Hostname** is the machine name on the tailnet. It defaults to
  `<computer name>-astesia`. Press Return to save; a running node is renamed
  right away.

These settings are stored in `astesia-settings.json` in the node's state
directory.

The MCP server (`astesia-mcp`) runs its own node, because two processes cannot
share one node's state. When that node is not signed in, the connection error
contains the sign-in URL. Open the URL in a browser and retry. To sign in
without a browser, set `TS_AUTHKEY` to an auth key before starting the process.

## State and signing out

Node state lives in the Astesia data directory:

| Process | Directory |
| --- | --- |
| Desktop app | `<data dir>/com.astesia.app/tailscale` |
| MCP server | `<data dir>/com.astesia.app/tailscale-mcp` |

`<data dir>` is `~/Library/Application Support` on macOS and
`~/.local/share` on Linux. To sign out or switch accounts, use **Sign Out** in
**Settings → Tailscale**, then sign in again. Deleting the directory while
Astesia is closed also resets the node. `tailscale.log` in the same directory holds the node's
log for the current run.

To use a self-hosted control server such as Headscale, set
`ASTESIA_TAILSCALE_CONTROL_URL` before starting Astesia.

## How it works

Database drivers accept only a host and port. For each tailnet target, Astesia
opens a listener on `127.0.0.1` with a random port. Each connection to that
listener is dialed through the embedded node, and the driver connects to the
listener. The listener stays open until Astesia exits. Like an SSH local
port forward, any program on the same machine can connect to it.

## Limits

- Only macOS and Linux builds include the node. libtailscale does not support
  Windows.
- MongoDB connects to the single server given (`directConnection=true`).
  Replica set discovery and Redis Cluster or Sentinel are not supported,
  because they connect to member addresses that bypass the listener.
- ClickHouse over HTTPS is not supported. Use the HTTP port. The traffic is
  already encrypted by Tailscale.
- Certificate checks that require the original host name, such as PostgreSQL
  `verify-full`, fail because the driver connects to `127.0.0.1`.

## Building

The feature is behind the `tailscale` cargo feature, which builds libtailscale
from Go source. Building it needs Go 1.25 or newer on `PATH`:

```bash
cargo run --locked --features tailscale --bin astesia
```

Release packages for macOS and Linux enable the feature. The Nix build and the
default `cargo build` leave it off, so they do not need Go.
