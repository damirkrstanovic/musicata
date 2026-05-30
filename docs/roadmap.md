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

Status: complete.

Goal: stop rebuilding the entire library only in memory.

Tasks:

- [x] Add SQLite via SQLx.
- [x] Add initial schema migration.
- [x] Store providers, provider items, artists, albums, tracks, artwork paths, and scan state.
- [x] Keep provider mappings separate from canonical entities.
- [x] Add full rescan trigger with `--rescan` and `MUSICATA_RESCAN`.
- [x] Track file size, modified time, and scan errors in SQLite.
- [x] Set initial SQLite schema version with `PRAGMA user_version`.
- [x] Add incremental rescan detection on startup.
- [x] Add explicit incremental rescan command with `--scan-once`.
- [x] Add explicit incremental rescan API.
- [x] Track content hash where useful.
- Add non-blocking deep fingerprinting for large files so first import does not require reading every byte before saving.
- [x] Add explicit migration steps beyond schema version 1.
- [x] Keep local file paths out of canonical track IDs.

Done when:

- First scan populates SQLite.
- Restarting the server loads from the database.
- Full rescan can replace the stored library.
- Incremental rescan detects added, removed, and changed files.
- Local file paths are not used as canonical track IDs.

## Milestone 3: Metadata Extraction And Updates

Status: complete.

Goal: make local-library quality good enough for real collections.

Tasks:

- [x] Add Lofty for embedded audio tags.
- [x] Store observed metadata and canonical metadata separately.
- [x] Add metadata observation provenance: source, confidence, timestamp, and user approval state.
- [x] Add field-level provenance and review state for individual metadata values.
- [x] Read embedded title, artist, album, year, and track number with folder-derived fallback.
- [x] Extract title, album, artist, album artist, track/disc numbers, dates, genres, composers, embedded artwork, lyrics, MusicBrainz IDs, and ISRCs into observed metadata where present.
- [x] Preserve folder-derived fallback metadata for poorly tagged files.
- [x] Add MusicBrainz lookup by existing MBIDs.
- [x] Add MusicBrainz candidate search for unmatched albums and tracks.
- [x] Add metadata review/apply API shapes before building the UI.
- [x] Add safe metadata review/apply UI before bulk changes.
- [x] Add artwork selection and cache behavior.
- [x] Add Cover Art Archive artwork enrichment behind explicit candidate review.
- [x] Add sidecar `.lrc` and `.txt` lyric observations.
- [x] Keep tag write-back disabled by default.
- [x] Add generated MP3 tag fixture for scanner tests.
- [x] Add generated FLAC and MP4/M4A tag fixtures.
- [x] Add generated tag-heavy embedded metadata fixture.
- [x] Add generated poorly tagged file fixture for folder fallback coverage.
- [x] Group compilations under album artist and order multi-disc tracks by disc then track.

Done when:

- Test data and real libraries produce stable albums/tracks.
- Multi-disc albums and compilations are represented correctly.
- Metadata extraction failures are logged without aborting the scan.
- Musicata can enrich its database without modifying files.
- Users can preview metadata changes before applying them.

Reference: [Metadata Update Strategy](metadata.md)

## Milestone 4: Search And Browsing

Status: complete.

Goal: make the library fast to explore.

Tasks:

- [x] Add SQLite FTS5 full-text search (replaces the planned Tantivy dependency).
- [x] Add pagination and sorting to API endpoints.
- [x] Add browse endpoints for artists, albums, tracks, genres, years, folders, and recently added music.
- [x] Add metadata-based browse filters for genre, year, and composer.
- [x] Add album detail endpoints.
- [x] Add artist detail endpoints.
- [x] Support accent-insensitive and case-insensitive search where practical.

Done when:

- Large libraries can be searched without loading every track into API responses.
- Web UI uses paginated/detail endpoints instead of fetching everything.

Search design decisions:

- Tantivy was dropped as too complex: it is a second datastore to keep in sync
  with SQLite. SQLite FTS5 lives in the same database file and transaction.
- Search runs as SQL against FTS5 external-content indexes for artists, albums,
  and tracks (tokenized, ranked, prefix, accent-insensitive). It does not read
  the in-memory library snapshot, so it scales without holding everything in RAM.
- Indexes are kept current by triggers on the base tables, so any insert, update,
  or delete — including future incremental adds — is immediately searchable with
  no manual index maintenance.
- No application-level search cache: SQLite's own page cache already keeps hot
  index pages resident, and a separate cache would add invalidation complexity for
  little gain on a single-user local server. Revisit only if profiling shows a need.
- The list, detail, and browse endpoints now query SQLite directly rather than a
  resident in-memory library snapshot, so steady-state memory no longer scales with
  library size. The few low-frequency, per-entity operations (metadata review/apply,
  MusicBrainz lookups, artwork review) still load the library transiently for one
  request; converting those to targeted queries is a possible later refinement.

## Milestone 5: Playback, Queues, And Zones

Status: in progress.

Goal: move from “play this URL in browser” to server-managed playback state.

Tasks:

- [x] Add player registry — players are registered (reported to the server, e.g.
  from the web UI), persisted in SQLite, and survive restarts.
- [x] Add zone model — named groups of players used as a control target (a command
  sent to a zone applies to its players). No audio synchronization yet.
- Add queue model per player/zone. (MPD's own queue is currently driven directly;
  a server-owned persistent queue is still to come.)
- [x] Add commands: play, pause, stop, seek, next, previous, enqueue, clear,
  shuffle, repeat. (Reorder still to do.)
- [x] Add WebSocket state updates for controllers.
- [x] Add a web UI to register, name, zone, and control players (transport plus
  "play the current view on this player"), with live state over the WebSocket.
- [x] Add the local browser as a player provider. The browser player is a
  server-owned player: its queue, current track, and play/pause live on the
  server (so they survive a page refresh and stay in sync across controllers),
  and a browser tab renders the audio by driving an `<audio>` element from that
  state over a bidirectional WebSocket (reporting progress and track-ended back).
  One tab "owns" output at a time (coordinated client-side); refining handoff and
  folding it into the main footer is part of the player UX follow-up.
- Add playback session state and now-playing history. (Live now-playing is
  reported; durable history is still to come.)
- [x] Stop browser playback when the server-bound playback session heartbeat is lost.

Player provider design decisions (see also Milestone 10):

- The first local player reuses **MPD** over its native TCP control protocol —
  the de-facto "existing remote API" for headless Linux players — rather than a
  bespoke audio backend. Control is driven by the `idle` command, which pushes
  state changes so the UI stays current without polling.
- MPD plays Musicata stream URLs (`/api/tracks/{id}/stream`), so MPD needs no
  filesystem access and the same path works for any future networked player.
- Controllers receive live state over a **WebSocket** (`/api/players/{id}/ws`);
  commands go through the REST player API. MPD is configured via `--mpd`
  (`host:port[,host:port]`) and `--public-url`.
- MPRIS (D-Bus) is a future transport-only provider: it can pause/skip running
  desktop players but cannot reliably choose what plays. Emulating LMS/SlimProto
  or AirPlay (à la Music Assistant) is much later.

Player UX:

- [x] Refine **controlling** players: a transport bar in the footer with a seek
  slider + elapsed/duration, repeat/shuffle toggles, volume, an active-player
  selector, and a slide-up queue drawer with click-to-play, remove, and reorder.
- [x] Refine **showing** players: now-playing album art (with monogram fallback),
  live elapsed/seek position, and per-player volume.
- [x] Refine **adding** players: registration status + error feedback in the panel.
- [x] Add the **local web browser as a player provider**.
- [x] Fold the player controls into the main footer.
- Remaining: address validation/probe on add and player discovery; deeper mobile
  now-playing sheet; drag-and-drop queue reorder (currently move up/down). Folds
  into the Milestone 6 web controller upgrade.

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
- Visual polish pass on the existing controls — e.g. the library search field still
  reads too heavy/inelegant; refine inputs, spacing, and type throughout.
- Mobile now-playing experience: the player rail currently flows at the bottom of
  the stacked layout on small screens; design a proper compact/expandable
  now-playing sheet.

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
- Add a parametric EQ stage (per-band frequency/gain/Q biquad filters) to the
  per-zone pipeline, with a UI for editing bands. (CamillaDSP already implements
  parametric EQ, so this likely rides on the CamillaDSP integration below.)
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

Milestones 3 and 4 are complete, including SQLite FTS5 full-text search and the
move of all read endpoints onto SQL queries (the in-memory library snapshot has
been removed). The next implementation slice is Milestone 5: player registry,
queues, zones, and WebSocket state sync.
