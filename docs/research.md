# Open Source Music Server Research

Date: 2026-05-24

## Goal

Research existing open source and adjacent music systems that can inform a Roon-like open source music solution. The target product is a central server with strict separation between music sources, metadata, playback, players, and controllers. Initial support is local disk only, but the architecture must support future providers such as Spotify, Tidal, internet radio, and other catalogs.

## Summary Recommendation

Do not fork a single project as-is. The best direction is a greenfield core that reuses proven ideas and compatibility layers:

- Use a Roon-style split: Server, Control, and Output.
- Use OpenSubsonic/Subsonic as a compatibility API for existing clients.
- Learn player/zone behavior from Lyrion Music Server and Squeezelite.
- Learn provider/player abstraction from Music Assistant.
- Treat local disk as one `MusicProvider`, not as the domain model.
- Reserve a per-zone DSP pipeline for later, with CamillaDSP as a strong reference.

## Projects Reviewed

### Roon

Roon is proprietary, but it is the clearest product reference. Its architecture separates Server, Control, and Output. The server owns library management, metadata, queues, zones, decoding, and output streaming. Controllers are UI clients, and outputs are playback endpoints.

Useful reference: architecture, zones, per-output DSP, rich metadata, multi-device synchronized control.

Source: https://help.roonlabs.com/portal/en/kb/articles/architecture

### Music Assistant

Music Assistant is the closest conceptual match among open source projects. It has a central server, multiple music providers, player providers, queues, API control, Home Assistant integration, source linking, and broad player ecosystem support.

Useful reference: provider architecture, player provider abstraction, queue behavior, API shape, source linking.

Caveat: it is strongly connected to the Home Assistant ecosystem and has its own Python/runtime assumptions.

Sources:

- https://www.music-assistant.io/
- https://www.music-assistant.io/player-support/
- https://www.music-assistant.io/api/

### Navidrome

Navidrome is a strong local music server with low resource usage, large-library support, metadata scanning, web UI, multi-user features, multi-library support, and OpenSubsonic/Subsonic compatibility.

Useful reference: local library scanning, metadata import, web UI, OpenSubsonic compatibility, client ecosystem.

Caveat: it is primarily a personal-library server, not a generalized multi-source/multi-player architecture. Its plugin system is promising but currently focused on metadata agents, scrobblers, scheduled tasks, and event handlers rather than arbitrary streaming sources.

Sources:

- https://www.navidrome.org/docs/overview/
- https://www.navidrome.org/docs/usage/features/plugins/
- https://www.navidrome.org/apps/

### Lyrion Music Server

Lyrion Music Server, formerly Logitech Media Server, is the strongest open ecosystem for server-controlled audio players. It supports local music, internet radio, streaming services, Squeezebox-style endpoints, Squeezelite, controllers, player sync, and JSON-RPC/CLI control APIs.

Useful reference: player discovery/control, zones, Squeezelite endpoint model, server-side control API, mature controller/player ecosystem.

Caveat: mature but legacy Perl codebase. Better used as a design and protocol reference than as the main codebase.

Sources:

- https://lyrion.org/
- https://lyrion.org/players-and-controllers/
- https://lyrion.org/players-and-controllers/squeezelite/
- https://lyrion.org/reference/cli/introduction/

### Mopidy

Mopidy is an extensible Python music server with backends for sources and frontends for control. It supports local files, internet radio, and extensions for services such as Spotify, SoundCloud, and Tidal. It exposes HTTP, WebSocket, JSON-RPC, JavaScript, and MPD-compatible interfaces.

Useful reference: extension structure, provider/backend API, control API, MPD compatibility.

Caveat: it is closer to "one extensible player" than "central server with many independent output zones."

Sources:

- https://mopidy.com/
- https://docs.mopidy.com/latest/reference/

### OwnTone

OwnTone is an audio media server for local libraries, podcasts, audiobooks, internet radio, Spotify, AirPlay, Chromecast, DAAP, local playback, MPD clients, and JSON API control.

Useful reference: output adapters, AirPlay/Chromecast/DAAP/MPD integration, local playback.

Source: https://owntone.github.io/owntone-server/

### Ampache, Airsonic, Gonic

These projects are useful references for Subsonic/OpenSubsonic compatibility and long-running self-hosted music patterns. Ampache supports OpenSubsonic, Subsonic, native APIs, UPnP/DLNA, and DAAP.

Useful reference: Subsonic/OpenSubsonic API behavior and compatibility expectations.

Sources:

- https://ampache.org/api/subsonic/
- https://airsonic.github.io/docs/api/

### Jellyfin

Jellyfin is a general media server, not music-first, but its music documentation is useful for folder conventions, embedded metadata, disc handling, lyrics, artwork, and file format behavior.

Useful reference: music metadata conventions, artwork, lyrics, media library scanning.

Source: https://jellyfin.org/docs/general/server/media/music/

### Snapcast

Snapcast is a synchronized multiroom audio transport. It is not a standalone music server. It reads PCM from sources and distributes synchronized audio to clients.

Useful reference: synchronized multiroom transport, grouping clients, low-level endpoint distribution.

Source: https://github.com/snapcast/snapcast

### MusicBrainz Picard

Picard is the best reference for robust metadata identification: MusicBrainz IDs, AcoustID fingerprints, cover art, tag writing, scripting, and multi-format support.

Useful reference: metadata model, stable external IDs, acoustic fingerprint workflow, cover art.

Source: https://picard.musicbrainz.org/

### CamillaDSP

CamillaDSP is a strong open source reference for future DSP. It supports real-time processing, EQ, convolution, room correction, active crossovers, backends across platforms, WebSocket control, and Python automation.

Useful reference: future per-zone DSP pipeline and control model.

Source: https://www.camilladsp.com/

## Player And Controller Ecosystem

OpenSubsonic/Subsonic compatibility gives immediate access to many existing clients across Android, iOS, desktop, web, terminal, and embedded devices. Examples from Navidrome's catalog include Feishin, Ultrasonic, DSub, Substreamer, Submariner, Termsonic, Supersonic-style clients, and many others.

Lyrion/Squeezelite is the strongest model for server-controlled playback endpoints. Squeezelite is headless, supports gapless playback, wide sample rates, direct streaming for some plugins, and synchronized playback through LMS.

Snapcast is a candidate for synchronized audio transport if the project later needs reliable multiroom PCM distribution.

## API Direction

The project should expose a native API for first-party controllers and integrations. It should also implement OpenSubsonic compatibility for library browsing and streaming so users can use existing clients early.

Recommended API layers:

- Native WebSocket API for state, player updates, queues, and controller synchronization.
- Native HTTP/REST or JSON-RPC API for integrations and automation.
- OpenSubsonic compatibility API for existing mobile/desktop clients.
- Optional future bridge APIs for MPD, LMS/SlimProto, or UPnP/DLNA.

## Metadata Direction

Metadata should be central, not source-specific. Local files, Spotify, Tidal, and radio streams must map into the same domain entities where possible.

Recommended stable identifiers:

- Internal IDs for all entities.
- Provider mappings for each source.
- MusicBrainz IDs for artists, releases, release groups, recordings, and works when available.
- ISRCs for recordings when available.
- AcoustID fingerprints for local-file identification.

## Risks And Constraints

- Streaming services often restrict what third-party apps can do. Spotify and Tidal support must be treated as provider plugins with strict legal/API review.
- GPL/AGPL projects may be useful references, but copying code requires license review.
- Subsonic/OpenSubsonic clients expect specific behavior; compatibility should be tested against real clients, not only the spec.
- Multiroom sync is hard. Treat synchronized playback as a later capability unless a proven transport such as Squeezelite or Snapcast is adopted.
- DSP should be designed into the architecture now but not implemented in the first release.

## Initial Technical Direction

Build a small, strict core first:

1. Provider-neutral domain model.
2. Local filesystem `MusicProvider`.
3. Metadata scanner with embedded tags, artwork, lyrics, MusicBrainz IDs, and replay gain.
4. Library database with provider mappings.
5. Native control API.
6. OpenSubsonic read/stream compatibility.
7. Web controller.
8. One simple playback path, then add real endpoint/player providers.

## Rust Stack Research

The project should be Rust-first. This fits the server-heavy product shape: long-running service, provider plugins, metadata scanning, audio streaming, local indexing, and eventual endpoint software.

### Backend

Recommended backend stack:

- Tokio for the async runtime and background work.
- Axum for HTTP APIs, WebSocket/SSE, streaming routes, static assets, and middleware integration.
- Tower/Tower HTTP for shared middleware such as tracing, compression, authorization, CORS, and timeouts.
- SQLx with SQLite for the initial embedded database.
- Tantivy for embedded full-text search.
- `serde` for API and provider DTOs.
- `tracing` for structured logging.

Rationale: Axum is built around Tokio, Hyper, and Tower. Tokio is the standard async runtime ecosystem for Rust network services. SQLx gives async SQL with SQLite support for local-first deployment. Tantivy keeps search embedded and Rust-native instead of requiring Elasticsearch or Meilisearch.

Sources:

- https://tokio.rs/
- https://docs.rs/axum/latest/axum/
- https://github.com/launchbadge/sqlx
- https://docs.rs/tantivy/latest/tantivy/

### Audio And Metadata

Recommended initial crates:

- Lofty for reading tags and artwork from common formats.
- Symphonia for Rust-native audio decoding/demuxing research.
- FFmpeg only as an optional compatibility fallback if required by formats, transcoding, or browser playback support.

Sources:

- https://docs.rs/lofty/latest/lofty/
- https://docs.rs/symphonia/latest/symphonia/

### Web Controller

Native mobile apps are not required initially if the web controller is good enough. The primary client should be a responsive installable PWA.

Recommended first choice: Leptos with Axum. Leptos is a full-stack Rust web framework that can build browser-rendered SPAs, server-rendered apps, and progressively enhanced apps from the same Rust code. This makes it a good fit for a Rust-first web controller while still allowing explicit HTTP/WebSocket APIs for integrations.

Dioxus is the main alternative. It is attractive if one Rust UI codebase for web, desktop, and mobile becomes a higher priority. Its fullstack mode integrates with Axum and supports WebSockets and HTTP streams. For this project, Leptos is the better initial default because the requirement is a polished web app, not native mobile.

The web app should use:

- Rust/WASM UI where practical.
- Explicit HTTP/WebSocket APIs for library and playback operations.
- Browser `HTMLMediaElement` for initial browser playback.
- Media Session API for OS-level media controls where supported.
- PWA manifest and service worker for installability and resilient loading.
- CSS/design tokens for UI polish; avoid adding a JavaScript framework unless Rust UI development blocks product quality.

Sources:

- https://docs.rs/leptos/latest/leptos/
- https://dioxuslabs.com/learn/0.7/essentials/fullstack/
- https://developer.mozilla.org/en-US/docs/Web/API/HTMLMediaElement
- https://developer.mozilla.org/en-US/docs/Web/API/MediaSession
- https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest
- https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API/Using_Service_Workers

### Desktop And Mobile

Do not build native apps in the MVP. If a desktop package is later needed, a Tauri wrapper around the same web app is the most likely path because it keeps application logic in Rust and can reuse the web frontend.

Source: https://tauri.app/
