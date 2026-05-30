# Musicata

Musicata is an open source, Roon-like music platform for personal and shared music libraries. The goal is a central server that can discover music, enrich metadata, stream audio, manage players, and expose controller APIs without coupling the core model to any single music source.

Initial support will focus on local disk libraries. The architecture must still treat local files as one provider among many future providers, such as Tidal, Spotify, Qobuz, internet radio, network shares, or other catalogs.

## Architecture Direction

Musicata follows a strict Server / Control / Output split:

- **Server** owns library state, metadata, provider mappings, queues, zones, users, player registry, and APIs.
- **Music providers** expose searchable and playable media without leaking storage details into the domain model.
- **Player providers** expose independent playback endpoints and zones.
- **Controllers** are web, desktop, mobile, or automation clients that talk only through public APIs.
- **DSP** is deferred, but the playback pipeline should leave a per-zone insertion point for future processing.

## Rust-First Stack

The project should use Rust as much as practical:

- **Backend runtime:** Tokio.
- **HTTP/API server:** Axum with Tower middleware.
- **Persistence:** SQLite via SQLx for the first local-server release.
- **Search:** Tantivy for embedded full-text library search.
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

Recommended first choice: **Leptos + Axum**, using Rust/WASM for the web UI and explicit server APIs for playback, library, and integrations. Leptos is web-focused and supports SPA, server-rendered, and progressively enhanced modes from the same Rust code. Dioxus remains a strong alternative if sharing one Rust UI across web, desktop, and mobile becomes more important than web-first ergonomics.

## Initial MVP

- Central server.
- Local disk music provider.
- Library scan and incremental rescan.
- Metadata extraction, artwork, lyrics, MusicBrainz IDs where available.
- Search and browse by artist, album, track, and genre.
- Native HTTP/WebSocket APIs.
- Basic OpenSubsonic compatibility.
- Web/PWA controller.
- One working playback path with an independent queue.

## Running The Prototype

The server scans `testdata` by default and serves the web controller plus JSON APIs:

```sh
cargo run -p musicata-server
```

Use another library path or port when needed:

```sh
cargo run -p musicata-server -- --library /path/to/music --addr 127.0.0.1:3031
cargo run -p musicata-server -- --library /path/to/music --database .musicata/musicata.db --rescan
```

Configuration can also come from a config file or environment variables. Precedence is `defaults < config file < environment < CLI`.

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

- [Research](docs/research.md)
- [Initial Requirements](docs/requirements.md)
- [Roadmap](docs/roadmap.md)
- [Metadata Update Strategy](docs/metadata.md)
- [Listening History And Recommendations Research](docs/recommendations.md)

## Source References

- Axum: https://docs.rs/axum/latest/axum/
- Tokio: https://tokio.rs/
- Leptos: https://docs.rs/leptos/latest/leptos/
- Dioxus: https://dioxuslabs.com/learn/0.7/essentials/fullstack/
- PWA manifest: https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest
- Service workers: https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API/Using_Service_Workers
- Media Session API: https://developer.mozilla.org/en-US/docs/Web/API/MediaSession
- SQLx: https://github.com/launchbadge/sqlx
- Lofty: https://docs.rs/lofty/latest/lofty/
- Symphonia: https://docs.rs/symphonia/latest/symphonia/
- Tantivy: https://docs.rs/tantivy/latest/tantivy/
