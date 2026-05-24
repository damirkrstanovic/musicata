# Roadmap

Date: 2026-05-24

This roadmap starts from the current working prototype: a Rust workspace that scans `testdata`, exposes basic JSON endpoints, serves a browser controller, and streams local tracks to browser playback.

The main rule is architectural: local disk is the first provider, not the core model. Every milestone should keep music providers, metadata, playback, players, and controllers separated.

## Milestone 0: Prototype Baseline

Status: complete.

Current capabilities:

- Rust workspace with `musicata-core` and `musicata-server`.
- Local disk scanner over nested folders.
- Provider-neutral artist, album, and track structs.
- Axum/Tokio HTTP server.
- Serde-backed JSON API responses.
- Basic search over in-memory library data.
- Static web controller.
- Browser audio playback through streamed track URLs.
- Basic byte-range support for audio streams.
- PWA manifest and service worker skeleton.

Known limitations:

- No persistent database.
- No embedded tag parsing yet.
- No real queue or zone model.
- No WebSocket state sync.

## Milestone 1: Server Foundation

Status: complete.

Goal: replace prototype plumbing with the intended production server stack.

Tasks:

- [x] Move HTTP routing to Axum.
- [x] Use Tokio for async file streaming and the server runtime.
- [x] Replace hand-written JSON with `serde` DTOs.
- [x] Add `tracing` logs for startup, library scan state, and request summaries.
- [x] Add structured request/error output.
- [x] Add config loading from CLI flags, environment variables, and a config file.
- [x] Define stable API response shapes and error format.

Done when:

- Existing browser playback still works.
- `cargo test` covers health, summary, search, artwork, and streaming routes.
- The server can be configured without recompilation.

## Milestone 2: Persistent Library Database

Status: in progress.

Goal: stop rebuilding the entire library only in memory.

Tasks:

- [x] Add SQLite via SQLx.
- [x] Add initial schema migration.
- [x] Store providers, provider items, artists, albums, tracks, artwork paths, and scan state.
- Keep provider mappings separate from canonical entities.
- [x] Add full rescan trigger with `--rescan` and `MUSICATA_RESCAN`.
- [x] Track file size, modified time, and scan errors in SQLite.
- [x] Set initial SQLite schema version with `PRAGMA user_version`.
- [x] Add incremental rescan detection on startup.
- [x] Add explicit incremental rescan command with `--scan-once`.
- [x] Add explicit incremental rescan API.
- Track content hash where useful.

Done when:

- First scan populates SQLite.
- Restarting the server loads from the database.
- Full rescan can replace the stored library.
- Incremental rescan detects added, removed, and changed files.
- Local file paths are not used as canonical track IDs.

## Milestone 3: Metadata Extraction And Updates

Goal: make local-library quality good enough for real collections.

Tasks:

- Add Lofty for embedded audio tags.
- Store observed metadata and canonical metadata separately.
- Add metadata provenance per field: source, confidence, timestamp, and user approval state.
- Extract title, album, artist, album artist, track/disc numbers, dates, genres, composers, embedded artwork, lyrics, MusicBrainz IDs, and ISRCs where present.
- Preserve folder-derived fallback metadata for poorly tagged files.
- Add MusicBrainz lookup by existing MBIDs.
- Add MusicBrainz candidate search for unmatched albums and tracks.
- Add safe metadata review/apply UI before bulk changes.
- Add artwork selection and cache behavior.
- Keep tag write-back disabled by default.
- Add test fixtures for tag-heavy and poorly tagged files.

Done when:

- Test data and real libraries produce stable albums/tracks.
- Multi-disc albums and compilations are represented correctly.
- Metadata extraction failures are logged without aborting the scan.
- Musicata can enrich its database without modifying files.
- Users can preview metadata changes before applying them.

Reference: [Metadata Update Strategy](metadata.md)

## Milestone 4: Search And Browsing

Goal: make the library fast to explore.

Tasks:

- Add Tantivy for full-text search.
- Add pagination and sorting to API endpoints.
- Add browse endpoints for artists, albums, tracks, genres, years, folders, and recently added music.
- Add album detail and artist detail endpoints.
- Support accent-insensitive and case-insensitive search where practical.

Done when:

- Large libraries can be searched without loading every track into API responses.
- Web UI uses paginated/detail endpoints instead of fetching everything.

## Milestone 5: Playback, Queues, And Zones

Goal: move from “play this URL in browser” to server-managed playback state.

Tasks:

- Add player registry and zone model.
- Add queue model per player/zone.
- Add commands: play, pause, stop, seek, next, previous, enqueue, reorder, clear, shuffle, repeat.
- Add WebSocket state updates for controllers.
- Treat the browser player as the first player provider.
- Add playback session state and now-playing history.

Done when:

- Two browser tabs stay synchronized as controllers.
- A queue survives page refresh.
- Playback commands go through the server API rather than local UI-only state.

## Milestone 6: Web Controller Upgrade

Goal: make the web app good enough to defer native mobile apps.

Tasks:

- Decide whether to migrate the UI to Leptos now or keep static JS until server APIs stabilize.
- Build responsive views for library, album, artist, search, queue, player selection, and settings.
- Add Media Session API integration.
- Improve PWA installability, caching, loading states, and mobile ergonomics.
- Add virtualized lists for large libraries if needed.

Done when:

- The app is comfortable on phone and desktop browsers.
- Core playback and queue control do not require a native app.

## Milestone 7: Listening History And Recommendations

Goal: turn playback behavior into useful local discovery without compromising privacy.

Tasks:

- Record playback events: started, progress, completed, skipped, paused, resumed, loved, disliked, rated, queued, and playlist changes.
- Use the ListenBrainz completion rule as a default: count a listen after half the track or 4 minutes, whichever is lower.
- Persist history per user, track, player/zone, session, and playback source.
- Add stats views for most played, recently played, never played, skipped, favorites, and rediscovery.
- Add deterministic smart playlists before adding ML.
- Add metadata-based recommendations by genre, year, artist, album artist, composer, and MusicBrainz IDs.
- Add optional ListenBrainz scrobbling and recommendation import.
- Design an optional `musicata-ml` service for future audio embeddings, genre/mood inference, and similarity search.

Done when:

- Browser playback creates durable listening history.
- Users can disable or delete history.
- Musicata can generate useful local playlists without external services.
- ML is documented as optional and not required for playback.

Reference: [Listening History And Recommendations Research](recommendations.md)

## Milestone 8: Native API And OpenSubsonic

Goal: make Musicata useful beyond its first-party web app.

Tasks:

- Version the native HTTP/WebSocket API.
- Document API routes and event payloads.
- Implement basic OpenSubsonic endpoints for authentication, ping, artists, albums, songs, cover art, search, and stream.
- Test with real OpenSubsonic/Subsonic clients.

Done when:

- At least one third-party client can browse and stream from Musicata.
- Native API docs are accurate enough for integration work.

## Milestone 9: Provider And Plugin System

Goal: make the architecture ready for sources beyond local disk.

Tasks:

- Formalize `MusicProvider` capabilities.
- Add provider configuration and provider lifecycle.
- Keep local disk as the reference provider.
- Add internet radio as the first non-library provider.
- Evaluate plugin isolation: in-process Rust modules, subprocesses, WebAssembly, or external services.
- Document legal/API constraints for Spotify, Tidal, Qobuz, and similar services before implementation.

Done when:

- Adding a new provider does not require changing core domain structs.
- Providers declare capabilities and failure modes clearly.

## Milestone 10: Player Providers And Endpoints

Goal: support playback outside the browser.

Tasks:

- Define `PlayerProvider` and endpoint capabilities.
- Add a lightweight native endpoint prototype.
- Research and prototype Squeezelite/LMS bridge behavior.
- Research Snapcast for synchronized transport.
- Later evaluate AirPlay, Chromecast, UPnP/DLNA, and MPD integrations.

Done when:

- At least one non-browser endpoint can be controlled from the server.
- Browser and endpoint players share the same queue/zone command model.

## Milestone 11: DSP Preparation

Goal: reserve the right architecture without shipping DSP too early.

Tasks:

- Define a per-zone audio pipeline model.
- Add configuration placeholders for headroom, volume leveling, EQ, convolution, and sample-rate conversion.
- Research CamillaDSP integration.

Done when:

- Playback design can route decoded audio through a future DSP stage.
- No MVP feature depends on DSP being implemented.

## Milestone 12: Packaging, Security, And Operations

Goal: make Musicata installable and safe enough for real users.

Tasks:

- Add release builds for Linux first.
- Add systemd service examples.
- Add Docker or container image if useful.
- Add user authentication and local-network security model.
- Add backup/restore documentation for database and config.
- Add diagnostics for scan, metadata, provider, and playback failures.

Done when:

- A new user can install, point Musicata at a library, scan, browse, and play music with documented recovery paths.

## Immediate Next Steps

The next implementation slice should be Milestone 2:

1. Add content hashes where useful.
2. Add explicit migration steps beyond schema version 1.
3. Keep local file paths out of canonical IDs as the schema grows.
4. Start Milestone 3 metadata extraction once the remaining persistence cleanup is done.
