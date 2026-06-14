# Musicata

Musicata is an open source, Roon-like music platform for personal and shared music libraries. The goal is a central server that can discover music, enrich metadata, stream audio, manage players, and expose controller APIs without coupling the core model to any single music source.

Musicata supports local disk libraries, SMB network shares, and upstream OpenSubsonic/Navidrome servers as music sources today. The architecture treats each as one provider among many, so future catalogs (Tidal, Spotify, Qobuz, internet radio) slot in the same way.

## Architecture Direction

Musicata follows a strict Server / Control / Output split:

- **Server** owns library state, metadata, provider mappings, queues, zones, users, player registry, and APIs.
- **Music providers** expose searchable and playable media without leaking storage details into the domain model.
- **Player providers** expose independent playback endpoints and zones.
- **Controllers** are web, desktop, mobile, or automation clients that talk only through public APIs.
- **DSP** (parametric EQ, room & headphone correction) runs per output; the playback pipeline carries a per-zone insertion point. See [docs/dsp.md](docs/dsp.md).

## Rust-First Stack

The project should use Rust as much as practical:

- **Backend runtime:** Tokio.
- **HTTP/API server:** Axum with Tower middleware.
- **Persistence:** SQLite via SQLx for the first local-server release.
- **Search:** SQLite FTS5 for embedded full-text library search.
- **Metadata:** Lofty for audio tags and artwork extraction.
- **Decode/transcode research:** Symphonia first, with FFmpeg as an optional compatibility fallback if needed.
- **Observability:** `tracing` and structured logs.
- **Plugin direction:** Rust traits internally; evaluate WebAssembly or subprocess isolation before third-party plugins are enabled.

This is Rust-first, not Rust-only. Browser APIs, CSS, a minimal service worker, and optional external audio tools are acceptable when they are the pragmatic choice.

The current server uses Axum, Tokio, Serde, and `tracing` while keeping the core provider model in `musicata-core`.

## Web App Direction

Native mobile apps are not required for the initial product if the web app is good enough. The primary controller should be a responsive installable PWA with:

- mobile-first and desktop-friendly layouts;
- fast library browsing and search;
- now-playing and queue control;
- player/zone selection;
- Media Session integration for OS lock-screen/media controls where supported;
- service worker and web app manifest for installability and resilient loading.

The web app is built with **Svelte 5 + TypeScript + Vite**, compiled by `build.rs` and embedded in the server binary via `rust-embed`. It has two surfaces: the player (`/`) and the admin console (`/admin`).

## Features (0.9)

- Central Rust server with a SQLite-backed, incrementally-rescanned library.
- Music sources: local disk, SMB network shares, and upstream OpenSubsonic/Navidrome servers.
- Metadata extraction (Lofty), artwork fetching (iTunes / Deezer / Cover Art Archive / fanart.tv), MusicBrainz enrichment, and AcoustID fingerprinting — each on its own background worker.
- Full-text search and browse by artist, album, track, genre, year, composer, and folder.
- Playback queues and multi-player zones; MPD and Snapcast player backends plus browser playback.
- Per-output DSP (parametric EQ, room & headphone correction) and EBU R128 loudness leveling.
- Listening history with ListenBrainz similar-track radio and continuous play.
- Multi-user accounts with cookie sessions and per-user API tokens.
- OpenSubsonic API surface — Musicata both serves it and can consume another server.
- Native HTTP + WebSocket APIs and an installable Svelte PWA controller.
- Library export/import for backup and migration.

## Running The Server

The server scans `testdata` by default and serves the web controller plus JSON APIs:

```sh
cargo run -p musicata-server
```

Use another library path or port when needed:

```sh
cargo run -p musicata-server -- --library /path/to/music --addr 127.0.0.1:3031
cargo run -p musicata-server -- --library /path/to/music --database .musicata/musicata.db --rescan
```

User-facing settings (music sources, players, API keys, artwork fetching) live in the product — edit them live on the **/admin** Settings page; no restart, no config files. CLI flags and environment variables exist only for *bootstrap* (where the library and database live, the bind address), with precedence `defaults < config file < environment < CLI`.

```sh
cargo run -p musicata-server -- --config musicata.example.conf
MUSICATA_LIBRARY=/path/to/music MUSICATA_DATABASE=.musicata/musicata.db MUSICATA_ADDR=127.0.0.1:3031 cargo run -p musicata-server
```

On first run the server scans the configured library, reads embedded tags with Lofty, and stores canonical tracks plus provenance-aware observed metadata in SQLite. Later runs load from the database unless `--rescan` or `MUSICATA_RESCAN=true` is set.
By default, startup performs a lightweight incremental rescan check using provider item IDs, file sizes, modified timestamps, and content hashes. Use `--no-incremental-rescan` to load only from the database.
The running server can also rescan through `POST /api/library/rescan`; add `?force=true` to rewrite the stored library even when no changes are detected.

### Controlling an MPD player

Musicata can control a local [MPD](https://www.musicpd.org/) instance: it drives MPD over its native protocol, hands MPD Musicata stream URLs to play (so MPD needs no access to your files), and pushes live playback state to controllers over a WebSocket.

Start MPD with the sample config (it streams from Musicata, so its `music_directory` can be empty), then point the server at it:

```sh
mkdir -p /tmp/musicata-mpd/music
mpd --no-daemon docs/mpd.example.conf

# in another shell:
cargo run -p musicata-server -- --mpd 127.0.0.1:6600 --public-url http://127.0.0.1:3030
# or: MUSICATA_MPD=127.0.0.1:6600 MUSICATA_PUBLIC_URL=http://127.0.0.1:3030
```

`--public-url` is the address MPD uses to fetch streams from this server. `--mpd` is optional convenience seeding — you can also register players from the web app's **Players** panel (or `POST /api/players`), name them, group them into zones, and control them (transport plus "play the current view on this player"); registrations persist across restarts. Then:

```sh
curl localhost:3030/api/players
curl -X POST localhost:3030/api/players/mpd/commands \
  -H 'content-type: application/json' \
  -d '{"command":"play_tracks","track_ids":["<track-id-from-/api/tracks>"]}'
curl localhost:3030/api/players/mpd/state
```

A live integration test exercises the full path against a real MPD (it spawns MPD with a null output, serves a WAV at a Musicata stream URL, and drives playback). It is ignored by default since it needs the `mpd` binary; run it with:

```sh
cargo test -p musicata-server -- --ignored live_mpd
# set MUSICATA_MPD_BIN if mpd is not on PATH
```

Use `--scan-once` for a non-server scan/update command:

```sh
cargo run -p musicata-server -- --scan-once
cargo run -p musicata-server -- --scan-once --rescan
```

### Where artwork is stored

Musicata splits cover art into two cases:

- **Fetched / acquired covers** (from the artwork-provider lane — iTunes, Deezer,
  Cover Art Archive, fanart.tv). The image **bytes are cached as files on disk**, next to
  the database in a content-addressed, sharded layout:

  ```
  .musicata/artwork/<first-2-chars-of-key>/<cache_key>.<ext>
  ```

  The cache directory is derived from the database path (`<db parent>/artwork/`). The
  database stores only **provenance**, not the bytes — the `acquired_album_artwork` table
  records `album_id, provider, remote_url, cache_key, ext, width, status, acquired_at`,
  and `cache_key` maps a row to its file on disk. The table is intentionally foreign-key-
  less so a rescan's album rewrite can't wipe it; acquired covers are re-pointed back onto
  their albums after every scan.

- **Local / embedded covers** (a folder `cover.jpg`, or art embedded in the audio tags).
  These are **not copied** — `albums.artwork_path` points straight at the original file
  (the audio file itself for embedded art). Embedded art is extracted on demand at serve
  time and then cached in the same `.musicata/artwork/` directory.

Audio fingerprinting (AcoustID) feeds the first case: once an untagged track resolves to
MusicBrainz IDs, the id-exact providers (Cover Art Archive / fanart.tv) can download its
cover into that cache. Inspect it with:

```sh
ls .musicata/artwork/
sqlite3 .musicata/musicata.db "select album_id, provider, ext, status from acquired_album_artwork limit 10"
```

Useful endpoints:

- `GET /` web controller.
- `GET /api/library/summary` library counts.
- `POST /api/library/rescan` scan the configured provider and update the database when files changed.
- `GET /api/artists`, `GET /api/albums`, `GET /api/tracks` paginated, sortable lists (`?limit=&offset=&sort=`); each returns `{ items, total, limit, offset, sort }`.
- `GET /api/tracks?genre=Dub&year=2004&composer=Name&folder=Path` filtered track list.
- `GET /api/artists/{id}` artist detail with albums and tracks.
- `GET /api/albums/{id}` album detail with artist and track list.
- `GET /api/browse` metadata facets for genre, year, composer, and folder.
- `GET /api/browse/recently-added` tracks ordered by when they entered the database.
- `GET /api/search?q=darkwood&limit=50` ranked SQLite FTS5 search (tokenized, prefix, accent- and case-insensitive) across artists, albums, and tracks.
- `GET /api/players` registered players (e.g. MPD) with capabilities, zone, and online state; `POST /api/players` registers one (`{"address":"host:port","name":"..."}`).
- `PATCH /api/players/{id}` rename or assign a zone (`{"name":"..."}` and/or `{"zone_id":"..."}`, null to clear); `DELETE /api/players/{id}` removes it.
- `GET /api/players/{id}/state` current playback state (status, now playing, queue, volume).
- `POST /api/players/{id}/commands` issue a command, e.g. `{"command":"play_tracks","track_ids":["..."]}`, `{"command":"pause"}`, `{"command":"seek","position_seconds":30}`.
- `GET /api/players/{id}/ws` WebSocket stream of live playback state.
- `GET/POST /api/zones`, `PATCH/DELETE /api/zones/{id}`, `POST /api/zones/{id}/commands` manage zones and control all players in a zone.
- `GET /api/albums/{id}/artwork/cover-art-archive/candidates` reviewed remote artwork candidates for MusicBrainz-linked albums.
- `GET /api/metadata/write-back` current file tag write-back policy.
- `GET /api/tracks/{id}/stream` audio stream with basic byte-range support.

Run tests with:

```sh
cargo test --offline
```

If a sandbox or CI image has a read-only global Cargo registry, set `CARGO_HOME` to a writable directory before building.

## Documentation

- [Roadmap](docs/roadmap.md)
- [Prior Art](docs/prior-art.md)
- [Native + OpenSubsonic API](docs/api.md)
- [Metadata Update Strategy](docs/metadata.md)
- [DSP — EQ, room & headphone correction](docs/dsp.md)
- [Loudness (EBU R128)](docs/loudness.md)
- [Recommendations & Radio](docs/recommendations.md)
- [Continuous Play](docs/continuous-play.md)
- [Snapcast Transport](docs/snapcast.md)
- [Plugins](docs/plugins.md)
- [Web UI Style Guide](docs/style-guide.md)
- [Research](docs/research.md)
- [Initial Requirements](docs/requirements.md)

## Source References

- Axum: https://docs.rs/axum/latest/axum/
- Tokio: https://tokio.rs/
- Svelte: https://svelte.dev/docs
- PWA manifest: https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest
- Service workers: https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API/Using_Service_Workers
- Media Session API: https://developer.mozilla.org/en-US/docs/Web/API/MediaSession
- SQLx: https://github.com/launchbadge/sqlx
- Lofty: https://docs.rs/lofty/latest/lofty/
- Symphonia: https://docs.rs/symphonia/latest/symphonia/
- Tantivy: https://docs.rs/tantivy/latest/tantivy/
