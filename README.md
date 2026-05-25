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
Use `--scan-once` for a non-server scan/update command:

```sh
cargo run -p musicata-server -- --scan-once
cargo run -p musicata-server -- --scan-once --rescan
```

Useful endpoints:

- `GET /` web controller.
- `GET /api/library/summary` library counts.
- `POST /api/library/rescan` scan the configured provider and update the database when files changed.
- `GET /api/tracks` provider-neutral track list.
- `GET /api/tracks?genre=Dub&year=2004&composer=Name` filtered track list.
- `GET /api/browse` metadata facets for genre, year, and composer.
- `GET /api/search?q=darkwood` simple library search.
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
