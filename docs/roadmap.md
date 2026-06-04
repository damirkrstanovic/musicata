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
- Source-aware artwork cache (see prior-art §8). Cover bytes were served straight from
  the source on every request — fine for local disk, but a network source (SMB) was
  fetched over the wire per request (~0.3 s each; brutal on a large grid), and a
  missing/unreadable cover used to 500. Adopting the Navidrome/Jellyfin model: **DB
  keeps artwork provenance (source ref), bytes live in a local cache**
  (`.musicata/artwork/`), and **acquisition is a provider concern** (local reads the
  file; SMB fetches once and the cache holds it; embedded/Cover Art Archive later).
  Staged:
  - [x] **Missing/unreadable cover → 404** (UI shows its monogram fallback), never a 500.
  - [x] **SMB covers served through the provider** rather than a failed local-fs read.
  - [x] **Lazy cache population** — first request fetches the original via the provider
    and writes it to `.musicata/artwork/{ab}/{key}.{ext}` (sharded, atomic temp+rename);
    served from disk thereafter (cold ~0.31 s → warm ~0.001 s) and resilient to the
    source going offline. Local disk unchanged (already fast).
  - [ ] **Sized thumbnails** — a `?size=` param serving cached resized variants
    (e.g. 300 px grid, originals on demand) via the `image` crate. The real win for
    large-library grid/scroll; pairs with the M6 virtualized-list work.
  - [ ] **Content-hash keying + invalidation** — key cache entries by content hash
    (dedupes identical covers across compilations/various-artists) and include the
    source mtime so a changed cover invalidates automatically. Today keyed by the
    source-path hash with no mtime (a changed cover needs a manual cache clear).
  - [ ] **Eager prefetch + bounded cache** — optionally warm covers at scan time
    (Navidrome's CacheWarmer, deferred until after the scan transaction) and add an
    LRU/size cap (Navidrome defaults to 100 MB); Jellyfin keeps all.
  - **Extend acquisition to embedded tags + Cover Art Archive** —
    - [x] **Embedded artwork fallback.** When an album has no folder cover but a track
      carries embedded art, the scanner points the album's `artwork_path` at the audio
      file (`build_track`/`aggregate_track` in `musicata-core`; any track's cover fills
      the album) and emits an `artwork_url` so the UI requests it. The server's
      `album_artwork` handler detects the audio-file path, reads the file once (local
      disk or **SMB via `read_album_source_file`**), extracts the front picture
      (`musicata_core::extract_embedded_cover`, lofty), caches the image
      (`ArtworkCache`, content type sniffed from the bytes), and serves it — so a
      tagged-but-coverless library shows real covers instead of monograms. *Note:* the
      first request reads the whole audio file over the wire to extract; a future
      optimization is a header-range read. Eager extraction at scan time (the picture is
      already parsed for `embedded_artwork_count`) is also possible later.
    - [x] **External artwork providers — pluggable lane + automatic fill.** A pluggable
      `ArtworkProvider` lane (`crates/musicata-server/src/artwork_providers.rs`, mirroring
      the music-source registry) auto-fills coverless albums after each scan: it tries
      **Cover Art Archive** and **fanart.tv** (MusicBrainz-id keyed; fanart.tv joins when
      its free API key is entered in the `/admin` Settings panel) first, then **iTunes**
      and **Deezer** text search, skipping id-only providers when an album has no MBIDs.
      `artwork_fill_pass` (toggled in Settings — a DB-backed app setting, migration v20,
      not a flag — default on) downloads the cover, caches it, and writes
      an `acquired_album_artwork` row (migration v19) + the album's `artwork_url`; a
      `not_found` marker stops the periodic rescan from re-querying (weekly retry); the
      serve handler checks the acquired row first. Verified live: testdata's coverless
      albums auto-filled real 600×600 covers, persisting (and not re-fetching) across
      restart. ToS notes + the still-open `?size=`/content-hash items in prior-art §8.
    - [ ] **Manual override** — a "refresh / clear / replace artwork" action so a user
      can fix a wrong text-search match (the on-demand CAA candidate/review flow already
      exists for picking a cover; this would also clear/re-trigger an acquired one).

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
- [x] Add queue model per player/zone. The **browser player** owns a persistent
  server-side queue: queue items + playback row (status/position/elapsed/volume/
  repeat/shuffle) persist to SQLite (migration v17, two tables: `player_queue` +
  `player_queue_items`) and reload on startup, so a queue survives a server restart,
  not just a page refresh. A restored queue comes back **paused at its saved
  position** (no output tab renders audio at startup, so we never silently
  auto-resume). Queue-mutating commands rewrite both tables; playback-only commands
  update just the cheap single row; in-track elapsed is persisted from progress ticks
  throttled to once / 10 s rather than every ~1 Hz tick. **MPD's queue is now
  server-owned too**: `MpdPlayer` (`players.rs`) holds the same persisted `QueueState`
  as the browser player — Musicata owns the queue **content & order** (persisted to the
  `player_queue` tables, reconciled onto MPD per command) while **MPD owns the playback
  cursor** (which index is playing, elapsed, and its native shuffle/repeat/
  auto-advance, read back via `read_status`). On startup the **server queue wins**: it's
  restored (paused) and pushed onto MPD via `load_queue` (no autoplay; the saved
  position resumes on the first Play); with no persisted queue the server adopts MPD's
  current one. If an external MPD client edits the queue, the idle loop detects the
  unexpected queue-version bump and **re-asserts** the server queue. The queue thus
  lives in Musicata's DB for every player kind.
  **Per-*zone* queues** are now implemented: a zone owns its own canonical
  server-side queue, modeled exactly like the browser player (`ZonePlayer` in
  `players.rs`, persisted to migration v18 tables `zone_queue` + `zone_queue_items`,
  restored **paused** on startup). A zone is a first-class control target alongside
  players — `/api/zones/{id}/commands|state|ws` mirror the player surface (the WS loop
  is shared via a `QueueOutput` trait), and the web app's switcher lists zones in a
  "Zones" group, subscribes to the zone socket, and routes transport/queue/play
  actions through it (`commandTarget`). When a zone runs a command it updates its
  canonical queue and drives members: **browser** members render the zone's
  now-playing straight off the zone broadcast (the tab reports progress/ended back to
  the zone socket via `browserOutputsFor`), and **MPD** members are best-effort
  mirrors the command is forwarded to (queue ops map 1:1; indices stay aligned because
  MPD mirrors the zone queue). The zone's queue is the single source of truth; there
  is no audio sample-sync (deferred), so an **MPD-only zone** (no browser output
  reporting `ended`) can drift in position until the user hits next/previous —
  mapping MPD state back to the zone is a future improvement.
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
- [ ] **MPD authentication + secure transport** (deferred; see also Milestones 10
  and 12). Today Musicata only speaks to an unauthenticated MPD over plain TCP, so a
  password-protected or remote MPD can't be used safely.
  - **Password auth.** MPD's protocol has a single `password <plaintext>` command
    (sent after the `OK MPD` greeting); a wrong password returns `ACK [3@…]`, and a
    secured MPD rejects every command with `ACK [4@…]` until authenticated. Add an
    optional per-player password, following the **SMB-source credential precedent**
    (`SourceRecord` stores `username`/`password`; the API DTO redacts them): a
    `password` column on the `players` table (next migration), threaded through
    `RegisterPlayerRequest`/`PlayerRecord`/`MpdPlayer` → `MpdConnection::connect`
    (which must authenticate on **both** the command and idle-loop connections),
    redacted from the `/api/players` response, and accepted via `--mpd
    password@host:port` (the `MPD_HOST` convention). Validate on register by probing
    an authenticated connect (ties into the "address validation/probe" item above).
  - **Secure transport.** MPD has **no native TLS** (upstream declines it — Issue
    #297 — favouring an external envelope), and its password crosses the wire in
    clear text. Pragmatic answer for remote/untrusted networks: an **SSH tunnel or
    WireGuard/Tailscale VPN** (zero code; encrypts the password too) — document this
    as the recommended deployment. Optional native support, if wanted, means
    generalizing `MpdConnection`'s TCP-only stream into an enum
    `Plain(TcpStream) | Tls(tokio-rustls) | Unix(UnixStream)` so Musicata can dial an
    **stunnel TLS endpoint** or a same-host **Unix socket** (`local_permissions`, no
    password needed). `connect` is the single choke point, so this composes cleanly
    with the password work; the TLS path adds a `tokio-rustls` dependency (license
    check).

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
- A queue survives page refresh (and, for the browser player, a server restart).
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
- [x] Add virtualized lists for large libraries. Two layers: (1) track rows use
  `content-visibility: auto` with `contain-intrinsic-size`, so the browser skips
  layout/paint for off-screen rows; (2) **incremental (infinite-scroll) loading** so
  the app never pulls or builds the whole library up front. The old `loadLibrary` eager-
  loaded *every* track (~8.3 MB / ~0.8 s for an 11k-track library, then built 11k DOM
  rows and fired ~1.3k album-artwork requests) before first paint. Now the center track
  view, browse-filtered tracks, and search results page in 100 rows at a time via a
  shared `infiniteScroll` helper (an `IntersectionObserver` on a bottom sentinel pulls
  the next `/api/tracks?limit&offset` — or `/api/search?…&offset`, added for this —
  page; stops on a short page; no explicit page UI). The album sidebar keeps full album
  metadata in memory (small) but renders cards a chunk at a time, killing the artwork
  storm. Album open now fetches its tracks from `/api/albums/{id}` rather than filtering
  a full client-side track array. Browsing by genre/year/composer narrows the **album
  grid** too, not just the track list: `/api/albums` takes the same browse-filter params
  and (when set) returns only albums with a matching track (`list_albums_filtered`, an
  `EXISTS` over the tracks join), server-paged like everything else. Latency is asserted
  by the UI smoke suite's scale phase (windowed initial render, a one-page track fetch
  vs the old ~8.3 MB, interactive under 3 s, "scrolling appends more rows", and "browse
  filters the album grid").
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

- [x] Manual playlists and favorites (a foundation for the stats/smart-playlist work
  below). User-created playlists (ordered, CRUD) and favorites (starred tracks/albums/
  artists) are persisted (migration v13) and exposed three ways: the native API
  (`/api/playlists`, `/api/favorites`), the web app (Playlists sidebar with create/
  delete/add/remove, a per-track ♥ toggle, and a Favorites view), and the OpenSubsonic
  API (get/create/update/deletePlaylist, star/unstar, getStarred/2, `starred` flags) —
  verified against the real Supersonic client.
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

- [x] Formalize `MusicProvider` capabilities. `ProviderCapabilities`
  (`can_scan`/`can_browse`/`can_search`/`can_stream`) is advertised per source;
  callers skip work a source can't do (radio streams but never scans).
- [x] Add provider configuration and provider lifecycle. A `ProviderHandle` enum
  (enum-dispatch, like `PlayerHandle`) + `ProviderRegistry` hold the active sources;
  scannable sources merge into one library (`merge_libraries`, per-track
  `provider_id` preserved). Sources persist in a `sources` table (migration v15) and
  are managed at runtime via `/api/sources` (+ a Settings "Music sources" panel with
  capability chips). A `SourceFs` VFS lets the one scanner walk any backend.
- [x] Keep local disk as the reference provider (unchanged behaviour; now one
  provider among several).
- [x] Add internet radio as the first non-library provider. Stations (name + stream
  URL + optional homepage) persist (migration v14) and are exposed via the native API
  (`/api/radio`), the web app (a Radio sidebar section: add / play / remove, plus a
  "Browse radio" directory backed by the open Radio Browser API, proxied server-side),
  and the OpenSubsonic internet-radio endpoints (get/create/update/delete).
  A new `PlayerCommand::PlayStream{url,title}` plays an external stream directly on the
  browser and MPD players (no library resolution).
- [x] Promote internet radio to a real provider. Radio is now a built-in
  `ProviderHandle::Radio` (always present, like local-disk; not in the `sources` table)
  backed by the stations table. It advertises `STREAM_ONLY` capabilities
  (`can_browse` + `can_stream`) and implements the two non-scannable provider methods
  the plugin design called for — `browse() -> Vec<BrowseEntry>` and
  `resolve(item_id) -> StreamSpec` — on the async `ProviderHandle` layer (the core
  `MusicProvider` trait stays sync/tokio-free). Surfaced generically at
  `GET /api/sources/{id}/browse` and `/resolve?item=…`; the web Radio sidebar and
  Subsonic `getInternetRadioStations` read through it (station management stays on
  `/api/radio`). First exercise of the source-vs-transport split for a non-library
  source; template for future streaming-service providers.
- [x] Add an SMB/CIFS network share as the first remote-disk source. Read directly
  over the wire in pure Rust (the `smb` crate, no kernel mount, no libsmbclient),
  feature-gated as `provider-smb`. Scanning runs the shared scanner over an SMB
  `SourceFs` (a `Read+Seek`-over-`read_at` adapter with a read-ahead cache feeds
  lofty); streaming fetches only the requested byte range.
- Evaluate plugin isolation: in-process Rust modules, subprocesses, WebAssembly, or external services.
- [x] Document legal/API constraints for Spotify, Tidal, Qobuz, and similar services
  before implementation. Done (see prior-art §9; two fact-checked research passes).
  **Verdict:** every commercial service needs an unofficial, ToS-violating
  reverse-engineered client, and any DRM circumvention (Spotify/Tidal-HiFi/Deezer/Amazon)
  adds DMCA §1201 exposure that is sharper for an AGPL project publishing its source.
  So **build the open/DRM-free tier first** — and these become the next provider tasks:
  - [ ] **OpenSubsonic/Funkwhale upstream client** — consume Navidrome/Gonic AND
    federated Funkwhale pods (all speak Subsonic) as a source; lossless FLAC, no DRM,
    reuses our existing OpenSubsonic server knowledge. Highest leverage; do first.
  - [ ] **Podcasts (RSS / Podcast Index) + Internet Archive** — DRM-free, public APIs.
  - [ ] **Jamendo** (Creative Commons) — public REST API (free `client_id`),
    FLAC/OGG/MP3; `jamendo-rs` crate.
  - [ ] Commercial services, if ever: **opt-in, cargo-feature-gated, user-supplies-own
    credentials** (the `provider-smb` precedent). **Qobuz is the first target** (lossless,
    MD5 signing — no CDM needed; Rust prior art in `qobuz-api-rust`/`hifi-rs`/MoosicBox).
    Tidal is partial (lower tiers via `tidalrs`; HiFi is Widevine-gated). Spotify is
    deprioritized (librespot is mature Rust but lossy-only + Premium + account-ban risk +
    active anti-OSS enforcement). **Apple Music / Amazon Music are infeasible** for
    self-contained playback (MusicKit-locked / Widevine + a ToS that bans self-hosting).
    DRM-circumvention code, if any, stays out of the default build.
- Metadata-enrichment lane (distinct from sources): a metadata/plugin provider type,
  not a music source. **Started** — two instances now exist, both running as background
  passes after a scan, gated by Settings toggles:
  - the pluggable **artwork-provider lane** (`artwork_providers.rs`: iTunes/Deezer/Cover
    Art Archive/fanart.tv, priority + MBID capability, auto-fill; see M3 §8), and
  - **AcoustID audio fingerprinting** (`fingerprint.rs`: pure-Rust `symphonia` decode +
    `rusty-chromaprint`; identifies untagged tracks → MusicBrainz ids in
    `track_fingerprint`, migration v21, which the artwork lane then uses to reach the
    id-exact providers). Needs Musicata's own free AcoustID application key compiled in.
  Still open: auto-*applying* MusicBrainz metadata from those ids (retagging, with
  review/write-back), Last.fm/ListenBrainz scrobbling, Discogs lookups.

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
  can't be spoofed or hijacked. (Distinct from user↔server auth in Milestone 12.) This
  is the *endpoint→server* direction; the *server→upstream-player* direction (e.g.
  authenticating to a password-protected/TLS MPD) is scoped under Milestone 5's "MPD
  authentication + secure transport" item.
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
- Add user authentication and local-network security model. Document the recommended
  deployment for reaching remote services (MPD, SMB) over untrusted networks — an SSH
  tunnel or WireGuard/Tailscale VPN — since those protocols carry credentials in clear
  text and MPD has no native TLS (see Milestone 5's "MPD authentication + secure
  transport"). Note which stored secrets are plaintext at rest (SMB/MPD/Subsonic
  passwords) and decide whether to encrypt them.
- Add backup/restore documentation for database and config.
- Add diagnostics for scan, metadata, provider, and playback failures.

Done when:

- A new user can install, point Musicata at a library, scan, browse, and play music with documented recovery paths.

## Immediate Next Steps

Milestones 0–4, 6, and 8 are complete. **Milestone 5** is effectively done — its
"Done when" criteria are met and the server-owned queue model now covers all player
kinds: the browser player, **per-zone queues** (`ZonePlayer`, migration v18), and
**MPD's queue is server-owned** (Musicata owns content/order, MPD owns the cursor;
restored paused on startup, re-asserted over external edits). What remains in M5 is
polish — drag-and-drop reorder, player add validation/probe + discovery, deeper
mobile now-playing sheet — plus the deferred "MPD authentication + secure transport".

The next implementation slice is **Milestone 7 (Listening History And
Recommendations)**: the history foundation (the `listens` table, recently/most-played,
playlists, favorites) is in place, so the next work is richer playback events, the
ListenBrainz completion rule, stats views (never-played / skipped / rediscovery),
deterministic smart playlists, and metadata-based recommendations. Two high-value
side-quests outside the strict sequence: **sized artwork thumbnails** (`?size=`, the
open Milestone 3 staged item — biggest grid perf win) and the **OpenSubsonic/Funkwhale
upstream provider** (Milestone 9, flagged highest-leverage among new sources).
