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
   tar -xzf musicata-x86_64-linux.tar.gz
   cd musicata-x86_64-linux
   ./musicata-server --library /path/to/music --addr 0.0.0.0:3030
   ```

   Pick the archive that matches the machine: `musicata-x86_64-linux` (any 64-bit Intel/AMD,
   including low-power NAS CPUs), `musicata-x86_64-v3-linux` (AVX2 — faster fingerprinting on
   post-2015 CPUs, **SIGILLs on older ones**), or `musicata-aarch64-linux` (ARM). Each archive
   also carries `musicata.service`, `COPYING`, `NOTICE` and `THIRD-PARTY-NOTICES.md`.

2. Open `http://<host>:3030`, create the admin account, then add music sources and players
   from the **/admin** Settings page.

**Locked out?** If you forget the admin password and have no second admin to reset it from the
**/admin** Users panel, recover from the console with the server **stopped**:

```
./musicata-server --database /var/lib/musicata/musicata.db --reset-admin <username>
```

It prompts for a new password on stdin (min 8 chars), sets it — creating the account as an
admin if it doesn't exist — and exits without binding a port. Point `--database` at the same DB
the service uses. (The terminal echoes the typed password; clear your scrollback if that
matters.)

### As a systemd service

A sample unit ships in the archive (`musicata.service`); follow the install steps in its
header comment. It runs the server as a system user, keeps state (database + artwork cache) in
`/var/lib/musicata`, and binds the LAN.

### With Docker Compose (full stack)

`docker-compose.yml` runs the **server** (`:3030`) and the optional **audio-ML service**
(`musicata-ml`, "sounds-like" embeddings) together. It **pulls prebuilt images from GHCR** — no
source checkout or Rust build needed, just the compose file:

```
docker compose up -d              # pull ghcr.io/damirkrstanovic/musicata-{server,ml} and run
```

- **Pin a version:** `MUSICATA_TAG=0.9 docker compose up -d` (default `latest` = newest release).
  Images are published on each `v*` tag by `.github/workflows/docker-publish.yml`.
- **Build from source instead** (local changes / an unreleased commit): `docker compose up -d --build`.
- The GHCR packages must be **public** for an unauthenticated pull. If they're private, run
  `docker login ghcr.io` first (a GitHub token with `read:packages`).

Then open `http://<host>:3030`, create the admin account, and:
- add your music source (e.g. an SMB share) in **/admin** — read over the wire, **no host mount**;
- in **/admin → Settings**, enable audio analysis and set the service URL to **`http://ml:3091`**
  (the server reaches the `ml` container by name on the compose network).

State (DB, artwork cache, sources/credentials) persists in the `musicata-data` volume; the ML
model is cached in `musicata-ml-data` (downloaded once, needs internet on first run). A local
on-disk library can be dropped in `./music`; SMB/network sources don't need it. Snapcast
multi-room isn't included (it needs host audio devices) — see `docs/snapcast.md`.

### Remote access

Musicata is **LAN-first**: session cookies are not `Secure`, and MPD/SMB/OpenSubsonic source
credentials are stored in clear text. Reach it from outside your network over a VPN
(Tailscale/WireGuard) or an SSH tunnel — not the raw internet.

### Stored secrets at rest

A few secrets live in the database in clear text: the **SMB / MPD / OpenSubsonic source
passwords** you enter in Settings, and each user's **API token** (the Subsonic salted-token
scheme has to recompute it, so it can't be hashed). User login passwords are argon2-hashed and
session tokens are SHA-256-hashed, so those are *not* recoverable from the database.

These plaintext secrets are **deliberately not encrypted** today. Encryption only helps if the
key lives somewhere the database reader can't reach; without an OS keyring or hardware-backed
key the key would sit next to the database and the encryption would be decorative. The honest
mitigation, given the LAN-first posture, is **filesystem permissions**: the systemd unit runs as
a locked-down system user with state in `0750` `/var/lib/musicata`, so only that user (and root)
can read the database. Keep the state directory off shared/world-readable storage. Real
encryption is revisited if an OS-keyring integration lands. See
[decisions.md](decisions.md).

## Backup & restore

All durable state is two things under the state directory (next to the database, or
`/var/lib/musicata` under systemd):

- `musicata.db` — the SQLite library cache, players, playlists, favorites, settings, users,
  history, loudness, fingerprints, …
- `artwork/` — the content-addressed cover-art cache.

**Cold backup (recommended):** stop the server and copy the whole state directory (or snapshot
the filesystem). Restoring is the reverse — drop the directory back and start the server.

**Migrating to another machine:** use the in-product **library export/import** (Settings →
`POST /api/library/export` → download the archive; `POST /api/library/import` on the new
instance stages it for the next startup). The export bundles the database snapshot plus the
artwork cache, so the imported instance comes up with the same library and covers without
re-scanning. (Source passwords travel with it — treat the archive as a secret.)

## Diagnostics

Background work (scan, metadata enrichment, fingerprinting, artwork, loudness, source
discovery) reports through the **activity log**: `GET /api/activity` lists recent activities,
each with a `running` / `ok` / `error` status and a message, and `GET /api/activity/ws` streams
them live (this is what the `/admin` activity view shows). A failed scan or an unreachable
source surfaces there as an `error` activity rather than a silent stall. `GET /api/health`
reports the API/server version, the active provider, and the track count for a quick liveness
check.

## Running the tests

```sh
cargo test --offline
```

If a sandbox or CI image has a read-only global Cargo registry, set `CARGO_HOME` to a writable
directory before building. See [AGENTS.md](../AGENTS.md) for the full build/test guide.
