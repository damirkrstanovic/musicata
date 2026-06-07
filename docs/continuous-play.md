# Continuous play — autoplay & endless radio (research + plan)

Date: 2026-06-07

## Context

"I play something and then it keeps going with similar stuff" — Spotify-style **Autoplay** /
**Song Radio**. When the queue is about to run out, Musicata appends *similar* tracks so the
music never stops, endlessly, without repeating. This builds directly on the (shelved)
ListenBrainz similarity research in **`docs/recommendations.md`** — continuous play is the
concrete, motivating use case that un-shelves a focused slice of it: the music doesn't *stop*,
rather than a "browse similar" page.

**The engine rides on the planned "Similar & Radio" slice** (`docs/recommendations.md`):
`SimilarityProvider` trait + `ListenBrainzLabsProvider` (CC0, no key, MBID-in/out) +
`LocalSimilarityProvider` (Jellyfin genre/artist/era fallback) + an MBID→local matcher +
`similarity_cache` (migration v26). None of that exists yet; **it's the prerequisite.**

## Two affordances, one engine

Match the universal pattern (Spotify, Roon, Music Assistant all do these):

- **Autoplay toggle — "Keep the music going"** (per player/zone, DB-backed, editable live in
  the UI; config-in-product, not a flag). When on and the queue nears empty, append similar
  tracks forever, continuing *whatever* you were playing.
- **Action — "Start radio from this"** on a track / album / artist — an explicit endless
  station from a chosen seed.

Same engine; the only difference is **seed origin** and whether the seed's own tracks are
included on the first fire.

## The algorithm (synthesized from the majors)

Every implementation is the same loop; the good ones differ in two details that matter.

1. **Refill trigger.** When `upcoming = queue.len() - cursor` drops **below 5** and continuous
   play is active, schedule a background fill (debounced). Music Assistant's "Don't Stop The
   Music" uses exactly `< 5` (`controllers/player_queues.py`). **Never on the hot path** — a
   separate task coordinating via the queue/DB, per the decoupling convention.

2. **Seed = a *sliding window*, not the original track.** THE key trick that keeps it endless
   and stops it collapsing into a loop: each refill re-seeds from a sample of the
   **recently-played** queue items (MA samples ≤10 from history), preferring the most recent.
   The seed walks with the listener, so the station drifts through the similarity graph
   instead of orbiting one song. (Explicit "Start radio from this" pins the seed only on the
   first fire.)

3. **Candidate pipeline (first-hit-wins chain):**
   - Seed track recording-MBID → cached **LB Labs `similar-recordings`** (`algorithm=
     session_based_…limit_50_skip_30_threshold_15…` — the API already returns ~50 score-sorted,
     skip-filtered candidates; honor `X-RateLimit-*`; cache in `similarity_cache` with TTL +
     `not_found`).
   - **Resolve** MBIDs → local tracks (MBID join, then normalized title+artist). Drop unresolved.
   - Too few resolved → **expand** via `similar-artists` of the seed/recent artists → their
     local tracks (the LB Radio `artist:` stream).
   - No MBIDs / external off / still thin → **`LocalSimilarityProvider`** (Jellyfin genre/
     artist/era weights over the existing `genres` JSON + artist + year). Always available, no
     network. This is also the **cold-start** path.

4. **Filter + variety** (pure SQL/in-memory over existing tables — big quality win for little
   code):
   - Exclude tracks already in the queue or just played (drop-by-id).
   - **Recency window** from `listens` — exclude the same `track_id` played (`event_kind=
     'played'`) within ~7–30 days.
   - **Skip penalty** — demote/exclude `event_kind='skipped'` tracks for a cooldown.
   - **Per-artist cap** — ≤2–3 per rolling window, never two in a row (Troi's
     `PlaylistRedundancyReducer`).

5. **Selection = weighted random sample by LB `score`** (Navidrome's weighted chooser): bias
   to closer results, keep randomness so it's not the same deterministic top-5 each time.
   Append a batch (~10–25) so the queue stays ≥5 ahead.

6. **Endless, on-genre, non-looping** follows from the above: sliding seed + score-biased
   sampling keeps it close; per-artist/recency caps stop both tight loops and off-genre drift;
   a background expander tops up the pool before it runs dry. LB's **easy/medium/hard** is the
   explore/exploit dial — default to roughly *medium* (mostly close, some reach).

## Where it hooks into Musicata (exact anchors)

- **Refill hook (browser + zone):** right after `advance(&mut state, true)` in
  `BrowserPlayer::track_ended` (`players.rs:~1409`) and `ZonePlayer::track_ended`
  (`players.rs:~1629`), before `broadcast()`/`persist()`. Condition: continuous-play active
  and `cursor + N >= queue.len()`.
- **MPD needs special handling** — MPD auto-advances natively (no server `track_ended`). Add
  the near-end check to the **position-poll loop** (`players.rs:~1020`): if `pos + N >=
  queue.len()`, trigger a refill out-of-band.
- **Server-side append path (reuse what exists):** build `PlayerCommand::Enqueue { track_ids }`
  (`musicata-core/src/lib.rs`) and apply it via the existing `apply_to_queue_state` →
  `resolve_queue_items` (hydrates id → `QueueItem` with stream_url/title/artist/artwork,
  `players.rs:~1784`) → `broadcast()` + `persist()`. The server can enqueue *without* a
  controller command — exactly what autoplay needs.
- **Seed data per track:** recording MBID from `track_musicbrainz_metadata` /
  `track_metadata_observations.musicbrainz_recording_id`; `genres` (JSON) + artist for the
  local fallback. **Dedup source:** `listens` (`recently_played` / a "played since" query).
- **Candidate fetch task:** a new `recommendation_loop` (the `*_loop` pattern in `main.rs`)
  with a rate-limited `ListenBrainzLabsClient` (`ureq` in `spawn_blocking`, like
  `MusicBrainzClient`), warming `similarity_cache`. The refill reads the cache (never blocks on
  the network); a cold seed enqueues a fetch and uses the local fallback meanwhile.

Coverage: **browser ✓, zone ✓** (both via `track_ended`); **MPD ✓** with the poll-loop check.

## Build order (value per effort)

1. **[DONE] Similarity engine** — `similarity_cache` (migration v27); `recommendations.rs`
   with the **ListenBrainz Labs** `similar-recordings` client (rate-limited, cached, parser
   unit-tested) and the **local content fallback** (`similar_local_track_ids`: genre/artist
   overlap, Jellyfin weights — always available, no network); the MBID→local matcher
   (`track_recording_mbid` + `tracks_for_recording_mbids`) and recency dedup
   (`recently_played_track_ids`). The `similar_track_ids` service chains them first-hit-wins.
   *(The ListenBrainz live path is wired + parser-tested but not yet verified against the real
   API — needs tracks with recording MBIDs + network; the local fallback is what tests exercise.)*
2. **[DONE] "Start radio from this"** — `GET /api/tracks/{id}/radio` (seed + similar) + a footer
   `((•))` button; `playTrackIds`/`startRadio` in the web player.
3. **[DONE] Autoplay + `< 5` refill** — a decoupled `autoplay_loop` (global `autoplay` setting,
   `GET/PUT /api/autoplay`, an "Autoplay" toggle in the queue drawer) that tops up a playing
   queue (browser + zones) with similar tracks when fewer than 5 remain, sliding the seed to the
   current track and excluding what's queued + recently played.
4. **[DONE] Per-artist cap** (≤2 per batch, round-robin so no artist dominates). **Next:** a
   skip-penalty cooldown (`event_kind='skipped'`).

### Fixes after testing on a real (MBID-less) library

Testing against an 11k-track library surfaced that **its tags carry zero MusicBrainz IDs**, so
the ListenBrainz path never triggered and radio fell to the local genre/artist fallback —
"weird"/sparse picks (an artist's own tracks only). Fixed:

- **Primary source is now LB `similar-artists`** (vastly better coverage than `similar-recordings`,
  which is empty for most recordings — verified live) — and it uses its *own* algorithm enum.
- **Seed artist MBID is resolved from the artist *name* via a cached MusicBrainz search** when
  tags lack it, so radio works for MBID-less libraries (the common case). Verified: an "AIR"
  seed now yields AIR + DJ Shadow + Amon Tobin + Daft Punk + Massive Attack.
- **Explicit radio no longer applies the recency filter** (only autoplay does) — that was
  shrinking a station, and collapsing sparse seeds to a single track, as plays accumulated.

Deferred: personal LB collaborative-filter recs (`/1/cf/recommendation/...` — *experimental*,
needs opt-in LB scrobbling, recommendations.md Slice 3); Last.fm provider (user key);
audio-embedding similarity (`musicata-ml`).

## Verification

Unit tests: the candidate pipeline over `testdata/` (seed with an MBID → cached LB fixture →
resolve to local; no-MBID seed → local fallback); dedup/recency/artist-cap filters; weighted
sampler. A ui-smoke flow: enable Autoplay, play a short queue to its end, assert the queue
auto-extends and playback continues (and the hot path is undisturbed). Manual: "Start radio
from this" on a track yields a coherent, non-repeating, never-ending stream.

## Sources

Music Assistant `controllers/player_queues.py` (Don't-Stop-The-Music: `< 5` refill,
sliding-seed `random.sample(...,10)`, `target_size=25`, dedup); Troi LB Radio reference
(modes/weights/interleave, `PlaylistRedundancyReducer`); live `labs.api.listenbrainz.org/
similar-recordings` (score-sorted, `limit_50/skip_30/threshold_15`); Roon Valence; Spotify
Autoplay/Smart-Shuffle. Musicata anchors: `players.rs` (track_ended/advance/enqueue/poll),
`musicata-core` (PlayerCommand), `storage` (listens, track MBID/genres), `musicbrainz.rs`
(rate-limited client). Underlying similarity infra: `docs/recommendations.md`.
