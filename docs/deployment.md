# Running & deploying Musicata

How to run Musicata from source for development, deploy a release binary, control an MPD
player, and reach the server from outside your network. For day-to-day use you don't edit any
of this — music sources, players, API keys, and artwork settings all live in the product on
the **/admin** Settings page (live, no restart, no config files). Flags and environment
variables exist only for *bootstrap* (where the library and database live, the bind address).

## Running from source

The server scans `testdata` by default and serves the web controller plus JSON APIs:

```sh
cargo run -p musicata-server
```

Use another library path or port when needed:

```sh
cargo run -p musicata-server -- --library /path/to/music --addr 127.0.0.1:3031
cargo run -p musicata-server -- --library /path/to/music --database .musicata/musicata.db --rescan
```

Bootstrap settings can come from a config file, environment variables, or flags, with
precedence `defaults < config file < environment < CLI`:

```sh
cargo run -p musicata-server -- --config musicata.example.conf
MUSICATA_LIBRARY=/path/to/music MUSICATA_DATABASE=.musicata/musicata.db MUSICATA_ADDR=127.0.0.1:3031 cargo run -p musicata-server
```

On first run the server scans the configured library, reads embedded tags with Lofty, and
stores canonical tracks plus provenance-aware observed metadata in SQLite. Later runs load
from the database unless `--rescan` or `MUSICATA_RESCAN=true` is set. By default, startup
performs a lightweight incremental rescan check using provider item IDs, file sizes, modified
timestamps, and content hashes. Use `--no-incremental-rescan` to load only from the database.

The running server can also rescan through `POST /api/library/rescan`; add `?force=true` to
rewrite the stored library even when no changes are detected.

Use `--scan-once` for a non-server scan/update command:

```sh
cargo run -p musicata-server -- --scan-once
cargo run -p musicata-server -- --scan-once --rescan
```

## Controlling an MPD player

Musicata can control a local [MPD](https://www.musicpd.org/) instance: it drives MPD over its
native protocol, hands MPD Musicata stream URLs to play (so MPD needs no access to your
files), and pushes live playback state to controllers over a WebSocket.

Start MPD with the sample config (it streams from Musicata, so its `music_directory` can be
empty), then point the server at it:

```sh
mkdir -p /tmp/musicata-mpd/music
mpd --no-daemon docs/mpd.example.conf

# in another shell:
cargo run -p musicata-server -- --mpd 127.0.0.1:6600 --public-url http://127.0.0.1:3030
# or: MUSICATA_MPD=127.0.0.1:6600 MUSICATA_PUBLIC_URL=http://127.0.0.1:3030
```

`--public-url` is the address MPD uses to fetch streams from this server. `--mpd` is optional
convenience seeding — you can also register players from the web app's **Players** panel (or
`POST /api/players`), name them, group them into zones, and control them (transport plus "play
the current view on this player"); registrations persist across restarts. Then:

```sh
curl localhost:3030/api/players
curl -X POST localhost:3030/api/players/mpd/commands \
  -H 'content-type: application/json' \
  -d '{"command":"play_tracks","track_ids":["<track-id-from-/api/tracks>"]}'
curl localhost:3030/api/players/mpd/state
```

A live integration test exercises the full path against a real MPD (it spawns MPD with a null
output, serves a WAV at a Musicata stream URL, and drives playback). It is ignored by default
since it needs the `mpd` binary; run it with:

```sh
cargo test -p musicata-server -- --ignored live_mpd
# set MUSICATA_MPD_BIN if mpd is not on PATH
```

## Deploying a release

Musicata ships as a single static binary with the web app embedded — no runtime dependencies.
Tagged releases attach Linux binaries for x86_64 and aarch64.

1. Download and extract the archive for your architecture from the
   [latest release](https://github.com/damirkrstanovic/musicata/releases/latest):

   ```sh
   tar -xzf musicata-x86_64-unknown-linux-musl.tar.gz
   cd musicata-x86_64-unknown-linux-musl
   ./musicata-server --library /path/to/music --addr 0.0.0.0:3030
   ```

2. Open `http://<host>:3030`, create the admin account, then add music sources and players
   from the **/admin** Settings page.

### As a systemd service

A sample unit ships in the archive (`musicata.service`); follow the install steps in its
header comment. It runs the server as a system user, keeps state (database + artwork cache) in
`/var/lib/musicata`, and binds the LAN.

### Remote access

Musicata is **LAN-first**: session cookies are not `Secure`, and MPD/SMB/OpenSubsonic source
credentials are stored in clear text. Reach it from outside your network over a VPN
(Tailscale/WireGuard) or an SSH tunnel — not the raw internet.

## Running the tests

```sh
cargo test --offline
```

If a sandbox or CI image has a read-only global Cargo registry, set `CARGO_HOME` to a writable
directory before building. See [AGENTS.md](../AGENTS.md) for the full build/test guide.
