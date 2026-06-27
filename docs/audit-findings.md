# Code audit findings

Source-wide audit (2026-06-27) of correctness bugs, issues, and overcomplicated solutions.
Each entry has a stable ID for tracking. Bugs (`BUG-*`) carry a concrete repro written so it can
become a regression test. Check the box when resolved; note the fixing commit/PR next to it.

Severity legend: 🔴 bug (wrong/unsafe behavior) · 🟡 issue (suboptimal/fragile) · 🔵 overcomplicated.

**Status summary:** 15 bugs (✅ all 15 fixed with tests) · 38 issues (open) · 9 overcomplicated (open).

Verified clean (audited, no defect): browser playback hot path (now-title never re-renders on a
progress tick); all background `*_loop` workers (own queue, idle sleep, no coupling, no busy-spin,
panics don't poison shared locks); Snapcast RBJ biquad coefficients; `loudness.rs` NaN/-inf clamps;
WebSocket reconnect/teardown; no native `alert/confirm/prompt`; rust-embed cache headers + no path
traversal; config precedence (CLI > env > file > default) with live DB reload.

---

## 🔴 Bugs

Security-sensitive bugs are marked **[security]**.

- [x] **BUG-01 [security] Pre-auth panic DoS in Subsonic `percent_decode`** ✅ fixed: byte-safe `hex_byte` helper; test `percent_decode_does_not_panic_on_multibyte_after_percent` — `crates/musicata-server/src/subsonic.rs:95`
  Guard checks byte length (`i + 2 < bytes.len()`) but slices the `&str` (`&input[i+1..i+3]`); a
  multi-byte UTF-8 char after `%` slices off a char boundary → panic. Runs in `parse_pairs` *before*
  auth, so any client crashes the request task unauthenticated.
  **Repro:** `GET /rest/ping.view?x=%1€` (literal `€` = bytes `E2 82 AC`). At i=0 → `&input[1..3]`
  ends mid-`€` → panic. Expected: normal Subsonic XML response.

- [x] **BUG-02 [security] Panic in Subsonic `decode_hex` on multi-byte char** ✅ fixed: byte-safe `hex_byte` helper; test `decode_hex_does_not_panic_on_multibyte_char` — `crates/musicata-server/src/subsonic.rs:294`
  Validates only that byte-length is even, then slices `&hex[i..i+2]`; a multi-byte char makes `i+2`
  land off a char boundary → panic. Reachable on the auth path via `p=enc:`.
  **Repro:** with a user configured, `GET /rest/ping?u=admin&p=enc:a€` → `decode_hex("a€")` has even
  byte-len 4, `&hex[0..2]` ends inside `€` → panic. Expected: error code 40.

- [x] **BUG-03 [security] Zip Slip path traversal in backup import** ✅ fixed: `sanitize_relative` drops `..`/root/prefix components; tests `extract_import_rejects_zip_slip_artwork_entry`, `extract_import_keeps_normal_artwork_entries` — `crates/musicata-server/src/backup.rs:103`
  `extract_import` does `artwork_import.join(rel)` on zip entry names with no containment check.
  Admin-gated, but the archive is attacker-supplied data.
  **Repro:** zip with entry `artwork/../escaped.txt` (+ a `musicata.db` entry so `found_db` passes);
  call `extract_import` with `artwork_import=/tmp/x/artwork`. Actual: file written to
  `/tmp/x/escaped.txt`, outside the staging dir. Expected: write confined under the staging dir.

- [x] **BUG-04 [security] Endpoint leaks its Bearer token to look-alike hosts** ✅ fixed: pure `resolve_request` attaches the token only for relative (library) URLs; tests `lookalike_host_does_not_receive_token`, `external_stream_does_not_receive_token`, `library_relative_url_resolves_and_carries_token` — `crates/musicata-endpoint/src/audio.rs:56`
  `fetch` attaches the player token when `url.starts_with(&self.base_url)` — a prefix check, not
  scheme+authority. `stream_url` for radio/podcast can be external.
  **Repro:** `AudioPlayer::new("http://127.0.0.1:3030","secret")`; `fetch("http://127.0.0.1:3030.example.com/track")`.
  Actual: `Authorization: Bearer secret` sent to the attacker host. Expected: no auth header.

- [x] **BUG-05 Shuffle is a no-op for browser, zone, and Snapcast players** ✅ fixed: shuffled-order semantics (no repeats until exhausted, reshuffle on repeat-all), current track first. `QueueState.shuffle_order` walked by `advance`/`step_previous` (browser + zone) and `next_index` (Snapcast); seeded by a dependency-free SplitMix64; persisted across restart via a JSON `shuffle_order` column (migration 033). Tests: `build_shuffle_order_*`, `shuffle_order_valid_*`, `advance_and_previous_follow_the_shuffle_order`, `shuffle_advance_stops_at_end_without_repeat`, `shuffle_advance_reshuffles_at_end_with_repeat_all`, `next_index_follows_shuffle_order` (snapcast), and the storage round-trip in `save_player_queue`/`load_player_queue`. — `crates/musicata-server/src/players.rs:1847` (SetShuffle), `:1906` (advance), `:1997` (next_index)
  `SetShuffle` only stores+broadcasts the flag; server-owned `advance` and Snapcast `next_index`
  always step `index+1` and never consult `state.shuffle`. Only `MpdPlayer` actually shuffles.
  **Repro (BrowserPlayer test):** play 5 tracks, `SetShuffle{enabled:true}`, call `Next` ×4, collect
  `snapshot().queue_position`. Actual: 1,2,3,4 (sequential). Expected: a non-sequential permutation.

- [x] **BUG-06 Incremental scan reuses stale metadata for a small file edited in-place** ✅ fixed: `content_hash_allows_reuse` guard added to the fast path; test `incremental_rereads_small_file_changed_in_place_with_same_size_and_mtime` — `crates/musicata-core/src/lib.rs:905`
  Fast-path reuse in `build_track` compares only `file_size_bytes` + whole-second
  `modified_at_unix_seconds`. A fresh `content_hash` was computed for small files during discovery but
  is never compared against `previous.content_hash`, so a same-size in-place retag within the same
  mtime second is reused wholesale.
  **Repro:** `FakeFs` with one 10-byte file (mtime 1000) → `first`; second `FakeFs`, same
  path/size/mtime but different bytes/tags; `scan_source_incremental(&fs2, "src", Some(&first), …)`.
  Actual: track reused, old id + old hash. Expected: re-read, new hash/id.

- [x] **BUG-07 `merge_libraries` double-counts `artist.album_count`** ✅ fixed: album_count recomputed from the deduped album set; test `merge_does_not_double_count_album_count_for_a_shared_album` — `crates/musicata-core/src/lib.rs:1364`
  Artists union with `album_count +=`, but albums union by id (deduped). When the same album id
  exists in two sources, `album_count` sums while the album vector keeps one row.
  **Repro:** two `Library`s, each artist "A" → album "Best Of" → 1 track (identical names → identical
  ids). `merge_libraries([a,b])`. Actual: `merged.albums.len()==1` but artist `album_count==2`.
  Expected: both 1.

- [x] **BUG-08 Migrations are non-transactional with raw non-idempotent `ALTER`** ✅ fixed: migrations 023/024/030 add columns via the idempotent `ensure_column`; test `migrations_are_idempotent_after_crash_between_alter_and_version_bump` — `crates/musicata-storage/src/lib.rs:74` (runner); consts `:5521` (030), `:5603` (023), `:5613` (024)
  The runner auto-commits each statement then `set_user_version` separately. MIGRATION_023/024/030
  use bare `ALTER TABLE ADD COLUMN` (not the `ensure_column` helper). A crash after the ALTER commits
  but before the version bump permanently bricks startup with "duplicate column name" on re-run.
  **Repro:** temp db migrated through v22 (so `listens` lacks `event_kind`); manually
  `ALTER TABLE listens ADD COLUMN event_kind …` but leave `user_version=22`; reconnect. Actual:
  re-runs MIGRATION_023, errors "duplicate column name", `connect` returns `Err`. Same for v24/v30.

- [x] **BUG-09 `set_playlist_tracks` replaces tracks without a transaction** ✅ fixed: wrapped DELETE+INSERTs+UPDATE in a `pool.begin()` transaction (atomic, no read race); test `set_playlist_tracks_is_atomic_on_mid_batch_failure` — `crates/musicata-storage/src/lib.rs:3885`
  DELETE + per-row INSERTs + UPDATE as separate auto-committed statements (no `BEGIN`/`COMMIT`).
  Crash mid-loop half-populates; a concurrent reader sees an empty playlist between DELETE and
  INSERTs. `create_playlist` inherits it.
  **Repro:** create a 3-track playlist; task A loops `set_playlist_tracks(id,[t1,t2,t3])`, task B
  loops `playlist_track_ids(id)`. Actual: B intermittently returns a short/empty vec. Expected:
  always 0 or 3.

- [x] **BUG-10 One malformed `<enclosure>` fails the entire podcast feed** ✅ fixed: `Enclosure.url` now `Option`, skipped when absent; test `malformed_enclosure_skips_only_that_item` — `crates/musicata-server/src/podcast.rs:119`
  `Enclosure.url` is a required `String`, so a present-but-`url`-less `<enclosure>` makes quick-xml
  fail the whole `Rss` deser — every valid episode lost. The doc comment promises such items are
  *skipped*.
  **Repro (unit):** `parse_feed` on a feed whose one item has `<enclosure type="audio/mpeg" length="1"/>`.
  Actual: `Err`. Expected: `Ok` with that item skipped.

- [x] **BUG-11 AcoustID lookup sends a truncated (~120 s) duration** ✅ fixed: full duration read from container `n_frames`/`sample_rate`, falling back to decoded count; test `duration_is_full_track_length_not_the_fingerprint_window` — `crates/musicata-server/src/fingerprint.rs:49` (decode cap `:120`)
  `compute_fingerprint` derives duration from `samples`, but decoding stops at `FINGERPRINT_SECONDS`
  (120 s). For any track > 120 s the duration sent to AcoustID is ~120, not the real length — AcoustID
  uses duration to disambiguate, so matching degrades.
  **Repro:** extend `silent_wav` to 200 s; `compute_fingerprint(&silent_wav(200),"wav")`. Actual:
  `duration==120`. Expected: `200`.

- [x] **BUG-12 Permanent ListenBrainz 4xx retried forever, wedging the queue** ✅ fixed: `is_retryable_status` splits 429/5xx (retry) from other 4xx (drop batch so the queue drains); test `http_status_retry_classification` — `crates/musicata-server/src/scrobble.rs:111`, `:146`
  `submit_listens` maps every non-2xx to `Err`; `scrobble_pass` leaves the whole FIFO batch queued on
  any `Err`. A 400 (malformed listen) or 401 (bad token) is resubmitted every 30 s indefinitely and
  blocks all newer listens behind it.
  **Repro:** enqueue one listen with empty `track_name` (rejected 400). Actual: row never deleted,
  resubmitted every pass; newer listens never sent. Expected: 4xx parks/drops the row so the queue
  drains.

- [x] **BUG-13 ML model output names indexed by string → panic → permanent mutex poison** ✅ fixed: `analyze` looks outputs up via `.get()` and errors cleanly; `lock_recovered` recovers a poisoned lock so one bad request can't brick the service; test `lock_recovered_survives_a_poisoned_mutex` — `crates/musicata-ml/src/model.rs:50,53`, `main.rs:131`
  `analyze` indexes `outputs["embedding"]`/`outputs["clip_scores"]`; ort's `Index<&str>` panics if
  absent. Model path is user-configured. The panic occurs while holding `state.model`
  (`std::sync::Mutex`), poisoning it — every later `/analyze` returns 400 "model lock poisoned" until
  restart. One bad request bricks the service.
  **Repro:** load any ONNX model whose outputs aren't named `embedding`/`clip_scores`; POST valid
  audio to `/analyze`. Actual: worker panics, all subsequent requests fail "model lock poisoned".
  Expected: clean 4xx/5xx, service stays up.

- [x] **BUG-14 `getAlbumList`/`getAlbumList2` ignores `starred`/`random`/`byYear`** ✅ fixed: handler dispatches `starred`→`starred_albums`, `random`→`RANDOM()`, `byYear`→new `albums_by_year`; tests `albums_by_year_filters_to_range_and_orders_by_direction` (storage), `parse_year_range_requires_both_numeric_years` (subsonic) — `crates/musicata-server/src/subsonic.rs:601`
  `type` maps only to a sort column; `starred`/`random`/`byYear` fall into `_ => None` and return the
  default-ordered *full* list. `byYear` also ignores `fromYear`/`toYear`.
  **Repro:** `GET /rest/getAlbumList2?type=starred&size=10` with no starred albums. Actual: first 10
  albums of the whole library. Expected: empty `<albumList2/>`.

- [x] **BUG-15 Export-in-progress on mount never starts polling** ✅ fixed: `refresh()` now owns the poll lifecycle and starts polling when it observes a running export. Verified by `npm run check` + reasoning (no TS unit-test runner exists; the smoke harness can't deterministically hold an export mid-flight). — `crates/musicata-server/web/src/admin/ImportExportPanel.svelte:23,31`
  `refresh()` only ever clears the poll; the `setInterval` starts only inside `startExport()`.
  Mounting while an export runs (reload mid-export, second tab) shows "Building…" forever.
  **Repro (smoke):** start export, reload `/admin` while building. Actual: stuck disabled on
  "Building…". Expected: polls and flips to "Download".

---

## 🟡 Issues

### Playback (`crates/musicata-server/src/players.rs`)

- [x] **ISS-01 Explicit `Next` on the last track restarts it** — `:1906` ✅ `advance` decides the next position before mutating; an unmovable explicit Next is a no-op. Test `next_on_last_track_without_repeat_does_not_restart` (+ `track_end_on_last_track_without_repeat_stops`).
  `advance` sets `elapsed=0` before checking it can move; on the last track (repeat≠All) the result is
  "same track, Playing, elapsed reset" instead of no-op/stop. *Repro:* 2-track queue, repeat Off, play
  pos 1, `report_progress(50,180)`, `Next` → snapshot shows pos 1, elapsed `Some(0.0)`.

- [x] **ISS-02 `Previous` doesn't wrap with repeat-All** — `:1834` ✅ fixed in the BUG-05 `step_previous` refactor (repeat-all wraps to the last item). Test `previous_wraps_to_last_with_repeat_all`.
  At index 0 with `RepeatMode::All`, `Previous` stays put; `advance` wraps in the forward direction.
  Asymmetric boundary handling.

- [x] **ISS-03 `Seek` is not clamped to `[0, duration]`** — `:1842` ✅ `clamp_seek` floors at 0 and caps at duration (also bounds the Snapcast frame cast). Test `seek_clamps_to_track_bounds`.
  Negative/past-end values are stored verbatim and propagate to controllers and the Snapcast frame
  cast (`:2245`/`:2263`).

- [x] **ISS-04 Removing the current item leaves stale `duration_seconds`** — `:1949` ✅ `remove_queue_item` clears `duration_seconds` when a new track shifts into the slot. Test `remove_current_item_clears_stale_duration`.
  Resets `elapsed=0` but, unlike `advance`, doesn't clear `duration_seconds`, so the new track is
  paired with the removed track's duration until an output reports a new one.

- [ ] **ISS-05 MPD idle-loop TOCTOU on `expected_playlist_version`** — `:999`/`:1182`
  The idle connection can read MPD's bumped playlist version before `execute` stores it, causing a
  spurious `reassert` on the server's own queue edit. Self-healing but can disturb playback.

- [ ] **ISS-06 MPD 5 s poll re-broadcasts full PlaybackState** — `:1131`
  No lightweight progress channel for MPD, so the poll calls `broadcast()` (full now-playing frame)
  every 5 s — the pattern the hot-path invariant warns against (only the browser path is smoke-tested).

### Subsonic / MPD (`subsonic.rs`, `mpd.rs`)

- [x] **ISS-07 `newest`/`recent` sort by release year, not date-added** — `subsonic.rs:603` ✅ new `albums_recently_added` orders by each album's max track add time (NULLs last); handler wires `newest`/`recent` to it. Test `albums_recently_added_orders_by_track_added_at_desc`.
- [x] **ISS-08 `getUser` returns the bootstrap single-user in multi-user mode** — `subsonic.rs:198` ✅ `requested_username` echoes the requested `username`/authenticated `u`. Test `getuser_reports_requested_then_authenticated_username`.
- [ ] **ISS-09 `f=jsonp` returns bare JSON with `application/json`, no callback wrap** — `subsonic.rs:54` ⏸️ DEFERRED — threading the callback string requires removing `Copy` from `Format` across ~144 sites for a niche legacy feature; disproportionate. Revisit if a JSONP client is actually needed.
- [x] **ISS-10 `escape_attr` doesn't numeric-escape newline/tab/CR** — `subsonic.rs:1318` ✅ `escape_attr` emits `&#xA;`/`&#x9;`/`&#xD;`. Test `escape_attr_numeric_escapes_whitespace_controls`.
  XML attribute-value normalization silently collapses them; correct is `&#xA;`/`&#x9;`/`&#xD;`.
- [x] **ISS-11 POST form bodies > 64 KiB silently dropped** — `subsonic.rs:124` ✅ raised the cap to a 4 MiB `MAX_FORM_BODY_BYTES` (covers large `createPlaylist`/`updatePlaylist`). Verified by reasoning + compile (no HTTP unit harness).
- [x] **ISS-12 Missing `u` with a configured password returns code 40, not 10** — `subsonic.rs:250` ✅ single-user `authenticate` returns 10 for a missing `u`, 40 only for a wrong one. Test `missing_username_with_configured_password_is_code_10`.
  Single-user path inconsistent with the multi-user path (which returns 10).
- [x] **ISS-13 MPD `seek` emits bare `seekcur N`** — `mpd.rs:134` ✅ `seekcur_arg` clamps to a finite, non-negative absolute value (no relative-seek or ACK). Test `seekcur_arg_is_absolute_finite_and_non_negative`.
  A negative value becomes a *relative* seek; NaN/Inf format as `NaN`/`inf` and draw an ACK.

### Storage (`crates/musicata-storage/src/lib.rs`)

- [x] **ISS-14 Multi-table deletes without a transaction** — delete_zone, delete_playlist, upsert_track_embedding ✅ each wrapped in a `pool.begin()` transaction. Covered by existing happy-path tests + the established atomic pattern.
- [x] **ISS-15 COMMIT-failure path returns the connection without ROLLBACK** — save_player_queue/save_zone_queue (+ save_library, replace_activities_once) ✅ all four COMMIT arms now ROLLBACK on commit failure.
- [x] **ISS-16 Unchecked `as usize`/`as u8` narrowing on persisted position/volume** ✅ position via `usize::try_from` (negative → None), volume clamped to 0..=100. Test `load_player_queue_rejects_corrupt_position_and_volume`.
- [x] **ISS-17 `try_get(...).ok()` silently drops rows on decode error** ✅ `similar_by_embedding`, `track_recording_mbid`, `track_artist_mbid` (and `tracks_for_recording_mbids`) now propagate with `?`.
- [x] **ISS-18 Missing deterministic tiebreaker → unstable pagination** ✅ `t.id` appended to `recently_played`/`most_played_since`/`most_skipped`. Test `most_played_breaks_ties_deterministically_by_track_id`.
- [x] **ISS-19 `tracks_for_recording_mbids` relies on un-ordered VALUES-CTE output** ✅ explicit ordinal + `ORDER BY s.ord`; errors propagated. Test `tracks_for_recording_mbids_follows_input_order`.

### Providers / sources

- [ ] **ISS-20 SMB cached client never invalidated on failure (no reconnect)** — `smb.rs:510`
  After a NAS reboot/blip all later requests fail until process restart. *Bug-class; needs a
  live/mock SMB server to repro.*
- [ ] **ISS-21 SMB `unwrap_file`/`unwrap_dir` panic on wrong resource type** — `smb.rs:443,486,626,639`
  Panics the request task if an id resolves to a dir (file op) or file (`read_dir`); `validate()`
  handles the same case gracefully. *Needs mock SMB.*
- [ ] **ISS-22 `join_smb_path` doesn't neutralize `..` or normalize backslashes in `base`** — `smb.rs:208`
  Possible escape above `base_path`; also inconsistent with `smb_provider_id` (`providers.rs:74`).
- [ ] **ISS-23 `scan_all` scans sources sequentially** — `providers.rs:360`
  One slow/offline SMB source stalls every other source's refresh (against the decoupling convention).
- [ ] **ISS-24 Radio Browser `ureq::Agent` has no timeout** — `radiobrowser.rs:60`
  A hung mirror ties up a `spawn_blocking` thread indefinitely; other providers set a 20 s timeout.
- [ ] **ISS-25 Podcast channel image ignores `itunes:image`** — `podcast.rs:88`
  Feeds that supply art only via `<itunes:image href=...>` return `image_url == None`.

### Metadata / enrichment

- [ ] **ISS-26 MusicBrainz has no shared cross-call/cross-thread rate limiter** — `musicbrainz.rs:12`
  Throttles only *within* a single multi-request call; the radio path + enrichment pass can exceed
  1 req/s and draw 503s (unlike AcoustID/ListenBrainz which carry a shared `next_slot`).
- [ ] **ISS-27 No backoff / `Retry-After` on MusicBrainz 503** — `musicbrainz.rs:366`
  All HTTP errors collapse to one string; callers retry immediately.
- [ ] **ISS-28 `download_image` trusts Content-Type, no magic-byte check** — `artwork_providers.rs:24`
  An HTML/JSON error page can be cached as `.jpg`.
- [ ] **ISS-29 iTunes art "upgrade" is a brittle string replace, always claims width 600** — `artwork_providers.rs:247`
  `replace("100x100bb","600x600bb")` no-ops when the token is absent, but still reports `width:600`.
- [ ] **ISS-30 Radio `by_artist` keyed by raw name but looked up lowercased** — `recommendations.rs:399`/`:314`
  Silent zero-contribution if `tracks_for_artist_names` isn't lowercased (test helper masks it). Verify
  the storage-layer behavior.

### Infra / audio

- [ ] **ISS-31 Login leaks valid usernames via timing** — `auth.rs:378`
  argon2 runs only for known users; unknown usernames return early (measurable difference).
- [ ] **ISS-32 No max password length → unbounded argon2 input** — `auth.rs:325`
  A multi-MB password on open `login`/`setup` burns CPU (cheap DoS lever).
- [ ] **ISS-33 AIMD limiter does non-atomic read-modify-write** — `scan_concurrency.rs:93`
  `limit` store + semaphore permit mutation aren't atomic under concurrency → drift between logical
  limit and actual permits.
- [ ] **ISS-34 `apply_staged_import` deletes the live DB before the replacing rename** — `backup.rs:123`
  `rename` already replaces atomically; if it then fails the live DB is gone and errors are only logged.
- [ ] **ISS-35 `wav_sample_rate` assumes `fmt ` at fixed offset 24** — `dsp.rs:140`
  Wrong for WAVs with `JUNK`/`LIST`/`bext` before `fmt `; bad rate stored as the IR sample rate.
- [ ] **ISS-36 Snapcast FIFO write failure drops the failed chunk after reopen** — `writer.rs:165`
  `cursor_frames` advances before `write_all`; ~20 ms of PCM lost on every reopen.
- [ ] **ISS-37 Player/endpoint token compared non-constant-time** — `auth.rs:238`
  Plain `==`/SQL `=` on sha256 hex; diverges from the password path (not practically exploitable).

### Frontend

- [ ] **ISS-38 Favorite toggle is optimistic with no rollback on failure** — `favorites.svelte.ts:21`
  UI silently lies vs server on a failed star/unstar (`autoplay.svelte.ts` rolls back; this doesn't).
- [ ] **ISS-39 `VuMeter` 60 fps rAF loop runs even when the drawer is closed** — `VuMeter.svelte:22`
  Mounted unconditionally (`App.svelte:293`); the loop isn't gated on `meter.open`.
- [ ] **ISS-40 Detail views stuck on "Loading…" on fetch failure** — `AlbumDetail.svelte:21`, `ArtistDetail.svelte:20`, `PlaylistView.svelte:20`, `SmartPlaylistView.svelte:20`
  `$effect` does `.catch(()=>{})`; no error state, nothing logged.
- [ ] **ISS-41 Sidebar search debounce timer not cleared on destroy** — `Sidebar.svelte:30`
  A pending 220 ms timer can fire after the component is gone (low impact).
- [ ] **ISS-42 `session.init` registers the unauthorized listener inside `init()`** — `session.svelte.ts:31`
  Listeners stack if `init()` runs more than once; also registered only after the `await` chain.

### Endpoint

- [ ] **ISS-43 Whole-track HTTP download runs on the single control-loop thread** — `endpoint/main.rs:185`
  Blocks pings/pause/stop/progress for the duration of the fetch; the server can see the channel as
  stalled.
- [ ] **ISS-44 Dropped session rebuilds `AudioPlayer`, restarting the track from 0** — `endpoint/main.rs:49`
  A transient blip stops audio and reloads from byte 0 instead of resuming.

---

## 🔵 Overcomplicated

- [ ] **CPX-01 First scan re-`read_dir`s each album folder once per track for artwork** — `core/src/lib.rs:989`
  N listings for N tracks; the discovery listing could be reused (bad for SMB).
- [ ] **CPX-02 `clean_optional_tag_value` rewrites `_`→space on embedded tag text** — `core/src/lib.rs:2078`
  Corrupts legitimate values ("AC_DC", "hip_hop"); underscore handling belongs only on the
  filename/folder inference path.
- [ ] **CPX-03 `StableHasher` conflates a `0` accumulator with "uninitialized"** — `core/src/lib.rs:2474`
  Re-seeds mid-chain when an intermediate hash is exactly 0 (rare collision smell in id generation).
- [ ] **CPX-04 `detect_library_changes` materializes whole observation tables for 3 counts** — `storage/lib.rs:741`
  Unbounded per-scan materialization growing with library size; empty-table early return also reports
  the wrong count.
- [ ] **CPX-05 MIGRATION_001 duplicates ~80 lines of DDL that 002–012 also build** — `storage/lib.rs:5074`
  Must be kept byte-compatible by hand or fresh-vs-upgraded schemas diverge (`duration_seconds` already
  inconsistent).
- [ ] **CPX-06 `stream`/`download` buffers the whole file in memory before ranging** — `subsonic.rs:711`
  `tokio::fs::read` then slices the range instead of seeking; the client side already streams in
  64 KiB chunks.
- [ ] **CPX-07 Snapshot-restore status-remap block copy-pasted across all four `restore` impls** — `players.rs:896/1429/1621/2125`
  A fix to the remap must be made in four places.
- [ ] **CPX-08 archive.org per-stem dedup is O(n²); `resolve` re-fetches the whole feed** — `archive_org.rs:170`/`:280`, `podcast.rs:232`
  A `HashMap` keyed by stem expresses the intent linearly; `resolve` downloads a full feed to find one id.
- [ ] **CPX-09 ML decodes+resamples the whole track, then keeps a 15 s excerpt** — `musicata-ml/main.rs:130`, `decode.rs:19`
  The cost the comment claims to avoid is paid before `center_excerpt` trims; decode could be bounded
  to the window.

- [ ] **CPX-10 `QueueDrawer` keys rows by array index, not item identity** — `web/src/player/QueueDrawer.svelte:36`
  Defeats per-item DOM reuse on reorder/remove. (Listed under overcomplicated; no correctness bug.)
