# Listening History And Recommendations Research

Date: 2026-05-24

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

