# Initial Requirements

Date: 2026-05-24

## Product Vision

Build an open source, Roon-like music platform centered on a local server. The server manages music discovery, metadata, search, streaming, playback zones, players, and controller state. Local disk is the first supported music source, but the system must be designed from day one for additional providers such as Spotify, Tidal, Qobuz, internet radio, network shares, and other catalogs.

## Architecture Principles

- Music source implementations must not leak into the core domain model.
- A track, album, artist, playlist, or stream must be represented independently from where it came from.
- Local files are one provider, not a special architectural case.
- Players connect to the server and are controlled independently.
- Controllers must use public APIs, not private database or filesystem access.
- DSP is deferred, but the playback pipeline must leave a clear insertion point for per-zone processing.
- Rust is the default implementation language for server, domain, provider, metadata, playback, and web UI code where practical.

## Stack Requirements

Musicata should be a Rust-first project, organized as a Cargo workspace once implementation begins.

Initial stack direction:

- Use Tokio for async runtime and background tasks.
- Use Axum for HTTP, WebSocket, streaming, and API routes.
- Use Tower/Tower HTTP middleware for tracing, compression, CORS, timeouts, and static assets.
- Use SQLite through SQLx for the first embedded database.
- Use Tantivy for local full-text search indexes.
- Use Lofty for audio metadata extraction.
- Evaluate Symphonia for audio decoding and stream inspection.
- Keep FFmpeg as an optional compatibility fallback, not the core abstraction.
- Use `serde` for API DTOs and provider/plugin contracts.
- Use `tracing` for structured logs.

Non-Rust exceptions are allowed for browser-required pieces such as CSS, PWA manifest files, and a minimal service worker.

Current implementation note: the server now uses Axum, Tokio, Serde, and `tracing` for the HTTP/API layer. Persistence, search indexing, and metadata extraction crates are still future milestones.

## Core Components

### Server

The server is the authoritative system for library state, metadata, queues, zones, users, player registry, provider configuration, and controller synchronization.

Initial server requirements:

- Run as a long-lived service on Linux first.
- Store metadata and playback state in a local database.
- Scan configured local music folders.
- Detect added, removed, and changed files.
- Stream playable audio to controllers or players.
- Expose native APIs for first-party apps and integrations.
- Expose an OpenSubsonic-compatible API for existing clients.

### Music Providers

Music providers expose searchable and playable music. The first provider is local disk.

Provider interface requirements:

- Browse artists, albums, tracks, playlists, and folders where supported.
- Search by artist, album, track, and free text.
- Resolve a provider item into playable media.
- Return artwork, lyrics, duration, format, and provider-specific IDs when available.
- Declare capabilities such as search, streaming, playlists, radio, recommendations, and write support.

### Metadata

The metadata layer merges file tags, provider metadata, and external metadata into provider-neutral entities.

Initial metadata requirements:

- Keep observed file/provider metadata separate from Musicata's canonical metadata.
- Track metadata provenance per field, including source, confidence, timestamp, and whether the user approved it.
- Read embedded tags from common formats such as FLAC, MP3, AAC/M4A, Ogg, and WAV where practical.
- Preserve multi-disc albums, compilations, album artists, composers, conductors, genres, dates, track numbers, and artwork.
- Store MusicBrainz IDs and ISRCs when present.
- Support local artwork and embedded artwork.
- Support synced and unsynced lyrics when present beside files or embedded.
- Keep provider mappings separate from canonical library entities.
- Enrich Musicata's database before offering any file tag write-back.
- Keep file tag write-back disabled by default and require a preview before applying changes.

### Playback And Players

Players are independent endpoints that connect to the server or are controlled through a player provider.

Initial playback requirements:

- Support at least one working playback path for the first release.
- Maintain independent queues per player or zone.
- Support play, pause, stop, next, previous, seek, volume, shuffle, repeat, and queue editing.
- Track now-playing state and playback progress.
- Keep the playback engine independent from local file paths.

Future player/provider targets:

- Native lightweight endpoint.
- Squeezelite or LMS-compatible bridge.
- Snapcast transport for synchronized playback.
- AirPlay, Chromecast, UPnP/DLNA, and MPD integrations.

### Controllers

Controllers are clients that browse music and control playback.

Initial controller requirements:

- Provide a responsive web controller as the primary client.
- Make the web controller installable as a Progressive Web App.
- Provide documented HTTP and/or WebSocket APIs for integrations.
- Keep UI state synchronized across multiple controller sessions.
- Allow selecting a player or zone and controlling its queue.
- Use browser media capabilities such as `HTMLMediaElement` first and add Media Session integration where supported.

Future controllers:

- Native mobile apps only if the PWA cannot meet key playback/control requirements.
- Desktop wrapper only if browser/PWA distribution is insufficient.
- CLI or automation client.
- Home Assistant integration.

## MVP Scope

The first useful release should include:

- Central server.
- Local disk music provider.
- Library scan and rescan.
- Metadata extraction and artwork support.
- Search and browse by artist, album, track, and genre.
- Streaming of local tracks.
- One web controller.
- PWA manifest and service worker for installability and resilient asset loading.
- One player/playback path.
- Independent queue for the active player.
- Native control API.
- Basic OpenSubsonic compatibility for existing clients.

## Deferred Features

- Spotify, Tidal, Qobuz, and other online providers.
- Multi-user permissions beyond basic admin/user needs.
- Multiroom synchronized playback.
- Advanced recommendations and radio.
- DSP, convolution, EQ, sample-rate conversion, and volume leveling.
- Offline downloads.
- Native mobile and desktop apps.
- Metadata editing and tag writing.

## Non-Functional Requirements

- The server must handle large local libraries without loading the entire catalog into memory.
- Scanning must be incremental after the first full scan.
- APIs must be versioned before external client development is encouraged.
- Provider plugins must run with explicit permissions and clear capability declarations.
- Logs must make scanning, provider failures, and playback failures diagnosable.
- The project must document license constraints before adapting code from GPL or AGPL projects.

## Open Questions

- Exact Cargo workspace layout.
- SQLite migration strategy and schema versioning.
- Audio decoding/transcoding stack.
- Whether the first endpoint is browser playback, a native local player, or a Squeezelite/Snapcast bridge.
- Plugin runtime model: in-process modules, subprocesses, WebAssembly, or external services.
- Web UI framework final choice: Leptos as the current front-runner, Dioxus as the main alternative.
