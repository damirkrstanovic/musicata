# Listening History And Recommendations Research

Date: 2026-05-24

> **Status — core shipped, follow-ups open.** Durable listening history, smart playlists,
> ListenBrainz similar-track radio and scrobbling, and the optional audio-embedding
> similarity all ship. Still open: richer playback events (loved/disliked/rated) and
> recommendation *import* from ListenBrainz — see roadmap M7. This document is the
> original research; it records options that were considered, not all of which were taken.

## Question

Can Musicata support recommendation engines by gathering listening history, using metadata, genre analysis, and optional machine learning?

Short answer: yes. The right path is incremental. Start with high-quality listening history and deterministic smart playlists, then add metadata/content similarity, then optional ML embeddings and collaborative filtering.

## Immich Lessons

Immich is a strong architecture reference because it adds ML without making ML part of the core request path.

Relevant patterns:

- The main app uses a client/server architecture with REST APIs and persistent database state.
- Background jobs handle expensive work such as metadata extraction, transcoding, smart search, and facial recognition.
- ML is externalized into a separate `immich-machine-learning` service.
- The ML service can run remotely or be disabled entirely.
- Model settings live in the database and are attached to ML requests by the server.
- Models are cached in the ML service after loading.
- Immich stores contextual search data in Postgres and currently uses VectorChord for vector search.
- Hardware acceleration is optional and configured separately.

Sources:

- https://docs.immich.app/developer/architecture/
- https://docs.immich.app/features/searching/
- https://docs.immich.app/administration/system-settings/
- https://docs.immich.app/features/ml-hardware-acceleration/
- https://docs.immich.app/guides/remote-machine-learning/
- https://docs.immich.app/install/upgrading/

## Musicata Recommendation Principles

- Listening history must be first-class product data, not just analytics.
- Recommendation features must be local-first and privacy-preserving by default.
- External scrobbling and recommendation providers must be opt-in.
- Recommendation inputs must use provider-neutral track/recording IDs.
- MusicBrainz IDs should be preferred whenever available.
- Local disk, streaming services, and radio must feed the same history model.
- ML must be optional and asynchronous. Playback must not depend on an ML service.

## Listening History Model

Musicata should record playback events before implementing advanced recommendations.

Recommended events:

- `play_started`
- `playback_progress`
- `listen_completed`
- `skipped`
- `seeked`
- `paused`
- `resumed`
- `loved`
- `disliked`
- `rated`
- `added_to_playlist`
- `removed_from_playlist`
- `queued`

Recommended event fields:

- user ID
- track ID
- provider mapping
- recording MusicBrainz ID when available
- album and artist IDs
- player/zone ID
- playback source: album, search, queue, radio, playlist, recommendation
- timestamp
- played duration
- track duration
- completion ratio
- session ID

Use the ListenBrainz rule as a practical default: count a completed listen when the user played half the track or 4 minutes, whichever is lower.

Sources:

- https://listenbrainz.readthedocs.io/en/latest/users/api/core.html
- https://listenbrainz.readthedocs.io/en/latest/users/json.html

## Recommendation Layers

### Layer 1: Smart Playlists

This needs no ML and should come first.

Examples:

- Most played this week/month/year.
- Recently added and unplayed.
- Rediscover tracks not played recently.
- Favorite albums.
- High-completion tracks.
- Albums started but not finished.
- Tracks often skipped.
- Genre, year, decade, artist, and album-artist mixes.

This provides immediate value and creates better feedback data for later stages.

### Layer 2: Metadata-Based Recommendations

Use existing metadata and MusicBrainz-linked data.

Signals:

- genre and style tags
- album artist
- composer
- release year and decade
- label
- country
- MusicBrainz relationships
- user tags
- playlist membership

Examples:

- More from this artist.
- Similar albums by genre/year.
- Same composer, different performer.
- Same label or scene.
- Genre radio.

### Layer 3: Behavior-Based Recommendations

Use listening history without requiring multi-user collaborative filtering.

Signals:

- completion ratio
- skip rate
- repeat listens
- recency
- session co-occurrence
- playlist co-occurrence
- user favorites/dislikes

Examples:

- Tracks often played in the same session.
- Albums likely to be replayed.
- "Play more like this queue."
- Personal daily mix.

### Layer 4: ListenBrainz And Troi Integration

ListenBrainz is the best open ecosystem for listening history and music recommendations. It supports submitting listens, fetching listens, recommendation endpoints, and recommendation feedback. Troi is MetaBrainz's playlist/recommendation playground and powers ListenBrainz playlist work.

Musicata should support:

- opt-in ListenBrainz scrobbling;
- importing ListenBrainz history;
- fetching ListenBrainz recommendations;
- resolving recommended MBIDs against the local library;
- using Troi ideas for local playlist generation.

Sources:

- https://listenbrainz.readthedocs.io/en/latest/index.html
- https://listenbrainz.readthedocs.io/en/latest/users/api/recommendation.html
- https://github.com/metabrainz/troi-recommendation-playground
- https://troi.readthedocs.io/en/latest/introduction.html
- https://troi.readthedocs.io/en/stable/lb_radio.html

### Layer 5: Audio ML And Embeddings

Audio ML can recommend by sound, mood, genre, tempo, and embeddings even when metadata is poor.

Essentia is the strongest open source reference. It provides audio analysis and music information retrieval, including descriptors, TensorFlow model integration, auto-tagging, classification, and embedding extraction.

Possible features:

- audio-derived genre/style tags;
- mood tags such as happy, sad, aggressive, relaxed;
- danceability, tempo, key, loudness;
- "sounds like this" recommendations;
- embedding-based similarity search;
- automatic radio seeds from a track, album, artist, mood, or genre.

Sources:

- https://essentia.upf.edu/documentation/
- https://essentia.upf.edu/documentation/tutorial_tensorflow_auto-tagging_classification_embeddings.html
- https://essentia.upf.edu/api/docs/tutorial/algorithms/

## ML Service Architecture

Follow the Immich pattern, adapted for Musicata:

- `musicata-server`: owns APIs, database, playback, queues, and scheduling.
- `musicata-worker`: runs background jobs such as scans, metadata extraction, stats aggregation, and recommendation refresh.
- `musicata-ml`: optional service for audio embeddings, mood/genre inference, and future model inference.

The ML service can be Python or Rust. Rust remains preferred for the core product, but Python is acceptable for the optional ML service if it gives practical access to Essentia or model tooling. The interface should be HTTP or another process boundary so the core server does not depend on Python at runtime.

## Vector Search Options

Do not require a vector database for the MVP.

Recommended sequence:

1. Store recommendation features and history in SQLite.
2. Use SQL/statistics for smart playlists and behavior scoring.
3. Add an embedded vector index only when audio embeddings are introduced.
4. Evaluate SQLite vector extensions for local-first deployment.
5. Evaluate Qdrant or Postgres + VectorChord only if scale or filtering requires it.

Sources:

- https://sqlite.org/vec1
- https://qdrant.tech/
- https://vectorchord.ai/
- https://github.com/pgvector/pgvector

## Data Privacy

Recommendation data can expose personal behavior. Defaults should be conservative:

- Keep history local by default.
- Make scrobbling opt-in per user.
- Let users delete listening history.
- Let users pause history collection.
- Separate local recommendation history from external submitted listens.
- Avoid uploading local file paths or private provider IDs to external services.

## Recommended Roadmap Impact

Add a dedicated milestone after server-managed playback state:

1. Record listening events from browser playback.
2. Persist history per user/player/session.
3. Add smart playlists and stats pages.
4. Add metadata-based recommendations.
5. Add ListenBrainz scrobbling/import.
6. Add optional ML worker and audio embeddings.

The first implementation should not start with ML. The most valuable next step is clean history capture, because every later recommendation layer depends on it.

---

# Implementation plan — Slice 1: Similar & Radio (2026-06-07)

History capture (the prerequisite above) is now **done**: `listens` (played/skipped,
the ListenBrainz completion rule), 4 deterministic smart playlists, favorites, and
recently/most-played all ship. This section is the concrete plan for the **next** slice,
informed by a deep prior-art pass over Immich, Jellyfin, Navidrome and the external APIs.

## Decisions (locked)

- **First slice = "Similar & Radio" recommendations**, not scrobbling or pure-local
  discovery. Matches the two guiding ideas: *use external "what people listen to" data*
  and *do what Jellyfin does locally*.
- **External similarity is ON by default**, with a Settings toggle to disable. It is
  *anonymous* (sends only MBIDs, receives CC0 data — no listening history leaves the box),
  background-fetched and cached, **never on a request's hot path**. This is the one
  intentional softening of the "external is opt-in" principle: it's anonymous + CC0, so the
  privacy cost is ~zero and the discovery payoff is high. The toggle still gives full opt-out.
- **Stay single-user.** History/favorites/recommendations remain global to the instance.
  No accounts. (Defers the roadmap's "per user" language; revisit only if multi-user lands.)

## Prior-art distilled (what we're copying)

- **ListenBrainz Labs** is the external source: `GET https://labs.api.listenbrainz.org/`
  `similar-artists/json?artist_mbids=…&algorithm=…` and `similar-recordings/json?…`. **MBID
  in → MBID out**, **CC0**, **no API key**, no attribution/commercial limits. Same engine
  behind LB Radio. Returns a `score` + `reference_mbid` per result. We already resolve both
  artist MBIDs (`artist_identity`, MBID-first) and recording MBIDs (fingerprinting v21 +
  MusicBrainz enrich v22), so resolution to local entities is a direct join. Rate-limited by
  `X-RateLimit-*` response headers — read them, don't hardcode.
- **Jellyfin** = the *local fallback*: weighted genre/tag/artist overlap scoring (their
  weights: genre 10, artist 30, tag 5, era 5) and genre-based "instant mix" (pick N audio in
  the seed's genres, randomize). Reimplementable as SQL over our existing `genres` JSON and
  artist columns — works with zero MBIDs and zero network.
- **Navidrome** = the *shape*: a first-hit-wins provider chain (mirrors our
  `ArtworkProviderRegistry`), an **MBID-first → normalized-name matcher** to map external
  results to local rows, and a **weighted chooser** for radio (collect similar artists' top
  songs, weight earlier artists higher, sample). Its buffered-scrobble design is the
  blueprint for Slice 3.
- **Immich** = the *eventual ML boundary* (separate HTTP service, DB-stored model settings,
  health checks, graceful skip) and the "precomputed nightly discovery surface" idea
  (memories → our rediscovery). Both deferred.
- **Last.fm**: better similarity quality but non-commercial-default ToS + attribution + no
  redistribution + flaky track MBIDs → **optional, user-supplies-own-key provider only**
  (Slice 4), never the default. **Spotify** removed related-artists/recommendations for new
  apps (Nov 2024) — dead. **AcousticBrainz** dump (CC0, frozen 2022) is useful only as a
  static bpm/key feature table later, not for similarity.

## Architecture — reuse over new code

> **As built (flatter than the trait-based plan below):** there is no `SimilarityProvider`
> trait, no `SimilarityRegistry`, no `LocalSimilarityProvider` type, and no
> `recommendation_loop`. Instead `crates/musicata-server/src/recommendations.rs` exposes a
> `similar_track_ids()` service that chains a `ListenBrainz` client + a local genre/artist
> fallback first-hit-wins, and the background pre-warm/refill is the **`autoplay_loop`**
> (`main.rs`). `similarity_cache` shipped at **migration v27**. The trait/registry framing in
> items 2–4 below was not built.

Mirror the **artwork-provider lane** (`crates/musicata-server/src/artwork_providers.rs`:
`ArtworkProvider` trait + `ArtworkProviderRegistry` in priority order) and the **decoupled
background-loop** convention (`*_loop` fns in `main.rs`).

New pieces:

1. **Storage (migration v27)** — a
   `similarity_cache(entity_kind TEXT, seed_mbid TEXT, payload_json TEXT, fetched_at INTEGER,
   PRIMARY KEY(entity_kind, seed_mbid))` table. `entity_kind ∈ {artist, recording}`. Payload
   is the scored MBID list. TTL-checked on read; a `not_found`/empty marker prevents re-querying
   (same pattern as `acquired_album_artwork`'s `not_found`). Add `get/set_similarity_cache`
   beside the existing `get_setting`/`set_setting` helpers (`crates/musicata-storage/src/lib.rs`).
2. **`crates/musicata-server/src/recommendations.rs`** —
   - `SimilarityProvider` trait: `similar_artists(seed_mbid) -> Vec<ScoredMbid>`,
     `similar_recordings(seed_mbid) -> Vec<ScoredMbid>`, plus a capability flag. First-hit-wins
     `SimilarityRegistry` (priority order), exactly like `ArtworkProviderRegistry`.
   - `ListenBrainzLabsProvider` — sync `ureq` in `spawn_blocking` with a rate limiter, like
     `MusicBrainzClient` (`musicbrainz.rs`). Honors `X-RateLimit-*`.
   - `LocalSimilarityProvider` — SQL genre/tag/artist/year weighted scoring (Jellyfin weights),
     always available; needs no MBID, no network.
   - `Matcher` — MBID-first (artist via `artist_identity` MBID / `musicbrainz_artist_id`;
     recording via `musicbrainz_recording_id`), else `normalize_artist_key` name match.
3. **Recommendation service** (in `recommendations.rs` or `main.rs`): `similar_artists(id)`,
   `similar_tracks(id)`, `artist_radio(id)`, `track_radio(id)`. Strategy: external-cached (if
   enabled + fresh) → resolve to local → blend `LocalSimilarityProvider` when too few results.
   Radio = Navidrome weighted-chooser over similar artists' local songs, seed excluded.
4. **Background `recommendation_loop`** — its **own** DB-coordinated task (per the
   "don't couple operations" convention; see [[decouple-background-operations]]), pre-warming
   the similarity cache for library artists that have MBIDs, draining then idle-polling, gated
   by the Settings toggle. A cold on-demand seed enqueues a fetch and the endpoint serves the
   local fallback immediately (never blocks).
5. **Native API**: `GET /api/artists/{id}/similar`, `/api/albums/{id}/similar`,
   `/api/tracks/{id}/similar`, `/api/artists/{id}/radio`, `/api/tracks/{id}/radio`. Returns
   scored **local** entities. Generate TS types via `scripts/gen-web-types.sh`.
   > **As built:** only `GET /api/tracks/{id}/radio` exists. The `…/similar` routes and
   > `/api/artists/{id}/radio` are not yet built.
6. **Web UI** (`crates/musicata-server/web/src/`): a "More like this" section + "Start radio"
   button on artist/album/track detail views, reusing the existing `TrackList`/album-grid
   components. A discover row ("Because you played …") can come in Slice 2.
7. **Settings**: one DB-backed toggle (default on) — "Fetch recommendations from ListenBrainz"
   — added to the `settings` table + `/admin` `SettingsPanel.svelte`. Gates external calls only;
   local recs always work.
8. **OpenSubsonic (bonus, infra now exists)**: `getSimilarSongs`/`getSimilarSongs2`,
   `getArtistInfo`/`2` (`similarArtist`), `getTopSongs` map 1:1 onto the new service and real
   clients use them.
   > **As built:** none of these OpenSubsonic endpoints are implemented yet.

## Later slices (sketch, not this PR)

- **Slice 2 — behavior-based discovery (all local)**: Daily Mix / "For you" seeded from
  history + expanded via similarity + recently-played excluded; session co-occurrence
  ("played after this"); rediscovery "on this day". Jellyfin "similar to recently
  played/liked" + Immich memories.
- **Slice 3 — outbound ListenBrainz scrobbling (opt-in)**: a `scrobble_queue` table + an
  independent `scrobble_loop` with backoff (Navidrome buffered-scrobbler), submitting
  `playing_now` + `single`. Unlocks personal CF recs (`/1/cf/recommendation/.../recording`).
- **Slice 4 — deferred**: Last.fm optional provider (user key, slots into the first-hit-wins
  chain); `musicata-ml` audio-embedding service (Immich boundary).

## Verification

- **Rust unit tests** (mirror `players.rs` + storage tests): `similarity_cache` round-trip
  + TTL/`not_found`; matcher MBID-first then name fallback; `LocalSimilarityProvider` SQL
  scoring over the `testdata/` fixture; `ListenBrainzLabsProvider` JSON parsing (fixture
  response, no live call); registry first-hit-wins. `cargo test` + `cargo build`.
- **Frontend**: `npm run check`; extend `scripts/ui-smoke.sh` with a "More like this opens +
  lists tracks" + "Start radio sets now-playing" flow.
- **Manual**: run against `testdata/`, hit `/api/artists/{id}/similar` and confirm LB Labs
  results resolve to local artists; toggle the setting off → endpoint returns local-fallback
  recs only and makes zero external calls (check the activity log / no outbound request).

