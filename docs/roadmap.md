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
- [x] Add now-playing history. Each player's state broadcast is watched by a
  per-player recorder that records a play the moment a (different) track starts
  playing — so a track shows up in history as soon as you play it (progress ticks,
  pauses, resumes, and seeks on the same track don't re-record; switching tracks
  does). Plays persist to a `listens` table (migration v11) and are kept for a
  rolling 30-day window (pruned hourly). `GET /api/history/recent` returns distinct
  tracks with their last-listen time; `/api/history/most-played` returns tracks with
  play counts. In the web app,
  "Recently played" shows relative times and is refreshed eagerly (it's a cheap
  indexed read — a 5s poll while open plus track-change events), while "Most played"
  shows play counts and, being a full aggregation, is loaded on demand and cached
  server-side for 60s so it isn't recomputed on every request. This is the foundation
  for Milestone 7.
- [x] Stop browser playback when the server-bound playback session heartbeat is lost.
- [x] No manual refresh anywhere. The server re-scans the filesystem on a 30s timer
  (incremental, so an unchanged library is cheap), and the web app polls the library
  summary (and re-checks on focus), reloading only when the track/album counts change.
  The history views also refresh on track-change events. The manual "Refresh" button
  is gone.

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

- [x] Decide whether to migrate the UI to Leptos now or keep static JS until server
  APIs stabilize. **Decision: keep the static vanilla HTML/CSS/JS app (no build step)
  for now.** The server APIs are still evolving (history, players/zones, metadata
  review are all recent), the current app already delivers the full controller
  experience offline-first, and a no-build static bundle keeps iteration fast and the
  PWA trivially cacheable. Revisit Leptos once the APIs stabilize and the JS starts
  straining (e.g. when complex client-side state or routing makes hand-written DOM
  updates costly).
- [x] Build responsive views for library, album, artist, search, queue, player
  selection, and settings. On narrow screens the shell collapses to a single column
  under a fixed top bar: the sidebar (search/browse/albums) becomes an off-canvas
  drawer (hamburger + scrim), the content goes full width, and the player moves to a
  bottom now-playing bar. Desktop keeps the three-column layout. (Settings and the
  metadata/queue overlays go full-screen on mobile.)
- [x] Add Media Session API integration. The web app publishes the active player's
  track metadata (title/artist/album/artwork) and playback/position state to the OS
  media surfaces (lock screen, media keys, notification, Bluetooth/car displays), and
  routes the OS play/pause/prev/next/stop/seek controls back to the active player.
- [x] Improve PWA installability, caching, loading states, and mobile ergonomics. The
  manifest now ships an icon (gold-monogram SVG, `any maskable`) plus id/scope/
  description/categories/display_override, so the app is installable; the icon is
  served and precached by the service worker. Initial and history loads show a
  shimmer skeleton (respecting `prefers-reduced-motion`).
- [x] Add virtualized lists for large libraries. Rather than a JS windowing library,
  track rows use `content-visibility: auto` with `contain-intrinsic-size`, so the
  browser skips layout/paint for off-screen rows. Revisit a true virtualizer only if
  this proves insufficient at very large library sizes.
- [x] Visual polish pass on the existing controls. The library search and the browse
  selects now share one lighter, translucent control treatment (no hard border until
  a soft gold focus ring), with small uppercase tracked field labels and custom
  dropdown chevrons.
- [x] Mobile now-playing experience: the bottom bar is a compact now-playing strip
  (art · title · play/pause) that expands to a full-screen sheet — big artwork, seek,
  full transport, player switch, volume, and queue — and collapses again.

Done when:

- [x] The app is comfortable on phone and desktop browsers.
- [x] Core playback and queue control do not require a native app.

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

- [x] Version the native HTTP/WebSocket API. The `/api` surface is **v1**; `GET
  /api/health` reports `api_version` and `server_version`. New fields/endpoints are
  additive within a version; the version bumps only on breaking changes.
- [x] Document API routes and event payloads. See [API reference](api.md) — native
  routes, player commands, the WebSocket `PlaybackState`/browser frames, and the
  OpenSubsonic surface (auth, formats, methods).
- [x] Implement basic OpenSubsonic endpoints for authentication, ping, artists,
  albums, songs, cover art, search, and stream. Mounted at `/rest`, XML + JSON, with
  Subsonic auth (plaintext / `enc:` hex / `t`+`s` MD5 token) against a configured
  `subsonic_user`/`subsonic_password` (open mode when unset, warned). Also
  getAlbumList/2, getGenres, getMusicFolders, getIndexes, and `scrobble` (which feeds
  Musicata's listening history). Unit + integration tested (auth, XML/JSON envelopes,
  browse, search3, stream bytes, cover art).
- [x] Test with real OpenSubsonic/Subsonic clients. Validated against two real client
  libraries: `py-sonic` (over JSON) and **Supersonic's own `go-subsonic` client** (over
  XML, Supersonic's default). Both run the full session — salt+token auth,
  reject-bad-password, getMusicFolders, getArtists, getIndexes, getArtist, getAlbum,
  getAlbumList/2, search3, stream (3.6 MB audio), scrobble — and pass. This surfaced
  two bugs hand-rolled curl tests missed: clients POST parameters in a form body (not
  just the query string), and getIndexes needs its own `<indexes>` wrapper; both fixed
  and regression-tested. Testing through `go-subsonic` exercises the exact networking
  code the Supersonic desktop app uses. Driving a full GUI client end-to-end (Supersonic,
  Symfonium, Amperfy, …) by hand remains a nice-to-have.

- [x] Broaden OpenSubsonic coverage toward Navidrome's surface (gap analysis done
  against Navidrome's 67 methods). Added `getMusicDirectory`, `getRandomSongs`,
  `getSongsByGenre`, `getLyrics`/`getLyricsBySongId` (served from stored lyrics), and
  advertised the `formPost` + `songLyrics` extensions. Tracks now carry **duration**
  (read from the audio stream at scan time; migration v12) so songs report `duration`
  and an approximate `bitRate`. Still missing (need new data models): playlists,
  favorites/ratings, play-queue sync, artist/album info, internet radio, jukebox.

Done when:

- [x] At least one third-party client can browse and stream from Musicata.
- [x] Native API docs are accurate enough for integration work.

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
- Introduce authentication between the server and players/endpoints. Today any device
  on the network can register itself as a player and any client can control one; the
  player-provider plugin interface should define how an endpoint proves its identity
  to the server (and the server to the endpoint) — e.g. a per-player token or shared
  key issued at registration and presented on the command/state channels — so players
  can't be spoofed or hijacked. (Distinct from user↔server auth in Milestone 12.)
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
