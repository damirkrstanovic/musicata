# Test coverage matrix

A feature-by-feature map of what is tested and at which layer, so gaps are visible. Update
this when you add a feature or a test.

**Layers**
- **Unit** — `cargo test` in `musicata-core` / `musicata-storage` and the pure helpers in
  `musicata-server` (parsers, biquads, query builders). Fast, no I/O.
- **API integration** — `musicata-server` `#[tokio::test]`s that build the real axum router
  (`TestFixture` → `app()`) and drive it with `oneshot(Request)`. This is the server-side
  end-to-end layer (HTTP in, JSON/bytes out, real SQLite + fixture library).
- **Browser e2e** — `scripts/ui-smoke.sh` → `tests/ui/v2-flows.mjs`, driving the built Svelte
  app over CDP/Chromium against the real `.musicata/musicata.db`. Two modes: `behavior` and
  `scale`. This is the only layer that exercises the actual UI + Web Audio + WebSocket.
- **Live** — `#[ignore]`d tests that hit real external services (AcoustID, MusicBrainz,
  ListenBrainz, a real MPD/OpenSubsonic). Run manually; never in CI default.

Status legend: ✅ solid · 🟡 partial / one layer only · ❌ none.

---

## Library, scan & browse

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Full + incremental scan, merge sources | `scans_*`, `incremental_scan_reuses_*`, `scanner_merges_normalized_*` (core); `saves_and_loads_library`, `detects_added_removed_and_modified_tracks`, `preserves_added_at_across_rescans` (storage) | `rescans_library_and_updates_state` | `library is large` (uses pre-scanned DB) | ✅ |
| Embedded/sidecar metadata (mp3/flac/m4a/lrc/txt) | `scans_embedded_*_fixture`, `scans_*_sidecar_lyrics`, `canonical_metadata_prefers_embedded_*` (core) | — | — | ✅ (unit) |
| Browse facets + filters | `browse_index_and_filters_metadata_facets` (core); `browse_index_reports_facets_from_sql`, `list_tracks_filters_and_paginates_in_sql` (storage) | `serves_browse_facets_and_filtered_tracks` | `browse filter changes the grid` | ✅ |
| Search (FTS, accent/prefix) | `search_*` (storage) | `serves_search_results_json` | `search at scale`, `search shows on segment`, `search persists across segment switch` | ✅ |
| Albums/artists listing + detail + pagination | `list_albums_filtered_*` (storage) | `serves_album_detail_json`, `serves_artist_detail_json`, `paginates_and_sorts_album_listing`, unknown→404 | grid render (implicit) | ✅ |
| Recently-added | — | `serves_recently_added_tracks`, `recently_added_orders_newest_first` | — | 🟡 (no e2e) |
| Artist merge / aliases | `regroup_applies_artist_aliases` (core); `artist_aliases_flatten_and_resolve` (storage) | `merge_and_unmerge_artists` | — | 🟡 (no e2e) |

## Artwork  ← *the area that just regressed*

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Album artwork serve / resize / ETag / 404 | `resize_to_jpeg_*`, `normalize_size_*`, `artwork_etag_*` | `serves_album_artwork`, `serves_resized_album_artwork_variant`, `missing_album_artwork_file_returns_404_not_500`, `detects_embedded_vs_folder_artwork_paths` | — | 🟡 (no e2e render) |
| Artist artwork serve | — | `artist_artwork_is_404_until_acquired` | — | 🟡 |
| Artwork acquisition (iTunes/Deezer/CAA/Fanart) | `itunes_*`, `deezer_*`, `caa_*`, `fanart_*`, `registry_skips_mbid_only_*` (providers) | — | — | ✅ (unit) |
| Acquired-art persistence (storage) | `acquired_artwork_lifecycle`, `acquired_artist_artwork_lifecycle`, `remap_identity_moves_favorites_and_artwork` (storage) | — | — | ✅ (unit) |
| **Acquired art survives a rescan / metadata edit** (`restore_after_scan`) | — | **`rescan_preserves_acquired_artist_artwork`** (added) | — | 🟡→ was ❌ (the bug: `/api/library/rescan` + metadata write-back skipped `restore_after_scan`) |
| Album artwork review/select | — | `reviews_and_selects_album_artwork`, `cover_art_archive_candidates_require_musicbrainz_ids` | — | 🟡 |
| **Artwork actually renders in the UI** | — | — | — | ❌ (smoke never asserts `<img>` loads) |

## Players, queue & playback

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Listen/skip recorder (threshold, cap, seek, pause, replay) | `crossing_half_duration_*`, `skip_when_track_changes_*`, `four_minute_cap_*`, `pause_resume_does_not_*`, … (players) | — | — | ✅ (unit) |
| Browser player play/queue/restart | `browser_queue_survives_a_restart_*`, `play_tracks_with_start_index_*`, `report_progress_emits_lightweight_tick_*` (players) | `browser_player_is_present_and_plays_tracks`, `playback_sessions_scope_browser_streams` | `playback started`, `queue drawer lists tracks`, hot-path ticks | ✅ |
| Track stream + HTTP Range | `parses_common_byte_ranges` | `serves_track_stream_ranges` | (playback implies) | ✅ |
| Zones (multi-room) | `zone_*` (players) | `zone_owns_a_queue_and_serves_state`, `registers_renames_zones_and_removes_players` | `switched output to a zone`, `zone plays` | ✅ |
| MPD player | `parses_status_*`, `round_trips_commands_over_tcp`, `mpd_player_restores_*` (mpd/players); `live_mpd_controls_playback` (live) | `offline_mpd_command_queues_track_and_reports_offline` | — | ✅ (no e2e — needs MPD) |

## History, recommendations, autoplay, radio

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Listening history aggregates | `listening_history_aggregates_*`, `listens_distinguish_plays_from_skips`, `frequently_skipped_*` (storage) | — | — | 🟡 (no route/e2e test for `/api/history/*`) |
| Similarity / weighted radio | `weighted_order_*`, `parses_similar_*` (recommendations); `listenbrainz_live_path` (live) | `track_radio` via route? — only `radio_native_and_subsonic` | `radio endpoint returns tracks`, `radio play sets now-playing` | 🟡 |
| Autoplay | — | — | `autoplay toggle persists` | 🟡 (logic `autoplay_candidates` untested at unit level) |
| Internet-radio stations | `radio_stations_round_trip` (storage) | `radio_native_and_subsonic`, `list_radio` | `radio station listed`, `radio button present` | ✅ |

## Identification & enrichment

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Fingerprint (Chromaprint) + AcoustID | `decodes_and_fingerprints_*`, `parses_acoustid_*`, `reserve_slot_*` (fingerprint); `live_acoustid_lookup` (live) | `identification_stats_and_unidentified`, `acoustid_lookup_reports_real_duration` | — | ✅ |
| MusicBrainz lookup/search/CAA | extensive `musicbrainz.rs` unit + `live_musicbrainz_enrichment` | `musicbrainz_lookup_returns_empty_*`, `musicbrainz_candidate_routes_skip_*` | — | ✅ |
| MB reapply survives resave | `musicbrainz_reapply_*` (storage) | — | — | ✅ |
| Metadata review + write-back policy | — | `metadata_review_api_updates_field_approval`, `metadata_write_back_policy_is_disabled` | `metadata panel opens` (open only) | 🟡 (no e2e edit; metadata-edit `restore_after_scan` untested) |

## Sources (providers)

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| SMB source | `joins_smb_paths`, `block_cache_coalesces_*`, `seek_and_read_*` (smb) | — | — | 🟡 (no route test) |
| OpenSubsonic upstream client | `md5_token_matches_*`, `parses_get_album_*`, `incremental_*` (opensubsonic); `opensubsonic_live_path` (live) | — | — | 🟡 (no route test) |
| Source CRUD + browse/resolve | — | `lists_sources_and_rejects_unknown_kind`, `delete_source_with_slashy_id_routes` | — | 🟡 (browse/resolve routes untested; admin UI not in smoke) |

## OpenSubsonic server (our API surface)

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Auth (token/password/open) | `md5_matches_known_vector`, `token_auth_*`, `plaintext_and_hex_*`, `open_mode_*` | `subsonic_ping_authenticates` | — | ✅ |
| XML/JSON rendering | `xml_renders_*`, `xml_escapes_*` | `subsonic_ping_renders_xml_by_default` | — | ✅ |
| Browse/search/stream/cover/lyrics | — | `subsonic_get_random_songs_*`, `subsonic_getartists_*`, `subsonic_search3_*`, `subsonic_stream_serves_*`, `subsonic_getcoverart_*`, `subsonic_get_lyrics_*` | — | ✅ |
| Playlists / stars / form-POST | — | `subsonic_playlists_and_stars`, `subsonic_reads_params_from_post_form_body`, `subsonic_advertises_formpost_extension` | — | ✅ |

## Favorites & playlists (native)

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Favorites / playlists round-trip | `playlists_and_favorites_round_trip` (storage) | `native_playlists_and_favorites_round_trip` | — | 🟡 (no e2e star/playlist-edit) |
| Smart playlists | — | `smart_playlists_list_and_detail` | `smart playlist opens + lists tracks` | ✅ |

## DSP / leveling / Snapcast

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| EBU R128 loudness analysis | `measures_a_sine_tone`, `louder_tone_measures_higher` (loudness); `leveling_gain_targets_*` (players) | — | `volume leveling boosts a quiet track` | ✅ |
| Browser EQ / response curve / leveling | — | — | `eq preset applies bands`, `eq biquads applied`, `eq graph processes audio`, `eq response curve renders` | ✅ |
| DSP profile shape + WAV parse | `profile_json_matches_the_web_client_shape`, `wav_sample_rate_reads_*` (dsp) | — | room-IR round-trip + `output switcher renders presets` | 🟡 |
| **DSP profile CRUD + impulse routes** (`/api/dsp/profiles`, `/impulse`) | — | — | partial (smoke PUTs one profile) | ❌ (no Rust route test) |
| Snapcast config render / auth / decode / in-process EQ | `render_config_*`, `authorization_block_*`, `decodes_*`, `resamples_*`, `low_shelf_dc_gain_*`, `preamp_scales_*` (snapcast) | — | `snapcast server-side DSP profile selection persists` | ✅ (no live streaming e2e — needs binary) |

## Auth (multi-user) & settings

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Password hash / tokens / path classification / cookie parse | `password_hash_round_trips`, `tokens_are_unique_*`, `admin_and_open_path_classification`, `cookie_parsing` (auth) | — | — | 🟡 |
| Users/sessions storage | `users_and_sessions_round_trip` (storage) | — | — | ✅ (unit) |
| **Auth endpoints** (setup/login/logout/me/token/password) | — | — | login UI bypassed (smoke seeds cookie via CDP) | ❌ (no route test) |
| **`require_auth` middleware gating** (401 without creds, admin-only paths, fail-open setup) | `is_admin_path`/`is_open_path` classify only | — | — | ❌ (no end-to-end gate test) |
| Settings get/update | `settings_round_trip` (storage) | — | persistence implied by autoplay/snapcast | 🟡 (no direct route test) |

## Infra / misc

| Feature | Unit | API integration | Browser e2e | Status |
|---|---|---|---|---|
| Config layering (file/env/cli) | `loads_default_config`, `layers_config_file_env_and_cli` | — | — | ✅ |
| Health | — | `serves_health_json` | — | ✅ |
| Error envelopes | — | `serves_stable_error_envelopes` | — | ✅ |
| Activity log | `activities_round_trip` (storage) | `activity_endpoint_returns_array` | — | ✅ |
| Activity persist under lock contention | `a_held_write_does_not_block_reads` (storage) | — | — | 🟡 (`replace_activities` retry/backoff untested) |
| **Library export / import** (`/api/library/export*`, import) | — | — | — | ❌ (no test at all) |
| SQLite scan concurrency governor | `grows_when_healthy_*`, `never_exceeds_max` (scan_concurrency) | — | — | ✅ |

---

## Browser (UI) flow coverage

The CDP smoke suite (`tests/ui/v2-flows.mjs`) only loads the **player page (`/`)**, in `behavior`
and `scale` modes. It is *not* Playwright — a raw CDP harness using Playwright's bundled Chromium.

**Covered (core music-playing is now solid):**
- Start playback (track row) + the now-playing hot path (elapsed ticks, title not swept).
- **Transport: pause → resume (same track), next, previous, seek** (added 2026-06-14).
- **Play an album from the grid cover** (paused → playing) (added 2026-06-14).
- **Favorite a track** (heart aria-pressed toggle) (added 2026-06-14).
- **Queue editing: reorder (move down), remove a row, clear** (added 2026-06-14).
- **Metadata review: approve a field** (✓ persists — PATCH + re-fetch reflects `active`) (added 2026-06-14).
- Queue lists tracks; browse filter; search (+persist across segments); smart playlist open.
- EQ (preset / curve / biquads / leveling / AutoEq / room IR), output switcher, Snapcast DSP, VU meter.
- Radio (endpoint, button, continue-seed, enqueue, play), autoplay toggle, zone switch + play.
- Album artwork renders (scale mode, real library).

**Not covered in the browser (deliberate / lower priority):**
- The **entire `/admin` page** (Sources, Settings, Users, Account, Players/Zones, Snapcast,
  Import/Export, Merged artists, Status) — the smoke never loads `/admin`.
- **Auth UI** — Login / first-run setup / logout (the smoke injects the session cookie via CDP).
- **Native playlists** — create / rename / reorder / delete / add-tracks (`PlaylistView`).
- **Album/Artist detail pages** — navigating into a detail view (play-from-grid *is* covered).
- **PWA install prompt**.

## Prioritised gaps (the backlog this file drives)

Closed in the 2026-06-14 coverage pass:

1. **✅ Acquired artwork survives rescan / metadata edit** — *root cause of the bug fixed
   2026-06-14.* `/api/library/rescan` and `update_track_metadata_field_review` saved the
   library without calling `restore_after_scan`, wiping artist (and acquired album) artwork +
   canonical grouping. → **`rescan_preserves_acquired_artist_artwork`** (verified it fails
   without the fix).
2. **✅ Auth endpoints + `require_auth` gating** — → **`auth_setup_then_gates_protected_routes`**:
   fail-open before setup → setup issues a working session cookie → protected route 401s without
   it → session + admin role grant access to a protected and an admin-only path.
3. **✅ DSP profile CRUD + impulse routes** — → **`dsp_profile_crud_and_impulse_round_trip`**:
   list → upsert (path-id wins, camelCase) → WAV-impulse upload/serve/delete → delete profile.
4. **✅ Library export / import** — → **`library_export_download_and_reimport`**: start the
   background export, poll to completion, download the zip, re-import it, assert it stages.
5. **✅ History routes** — → **`history_routes_reflect_recorded_listens`**: a recorded listen
   appears in `/api/history/recent` and `/most-played` (with the flattened track shape).
7. **✅ Artwork actually renders in the UI** — → smoke `album artwork renders (img bytes
   loaded)` in the **scale** phase (`naturalWidth>0` over the authenticated endpoint). Runs at
   scale because `testdata/` has no album covers; `v2-smoke.sh` symlinks the real artwork cache
   into the scale tmp dir so `?asset=` URLs resolve without copying it.

Still open (need infrastructure to test well — deferred deliberately, not forgotten):

6. **🟡 Source browse/resolve routes** — `/api/sources/{id}/browse` and `/resolve` need a
   configured source provider. SMB/OpenSubsonic require network; testing needs a mock/in-memory
   provider variant first. Unit-level provider behaviour *is* covered (`smb.rs`, `opensubsonic.rs`).
8. **🟡 `replace_activities` retry/backoff** (the `database is locked` fix) — exercising the
   retry deterministically needs fault injection (hold a write past `busy_timeout`); a naive
   contention test would be flaky and `busy_timeout` alone would mask the retry. The normal path
   is covered by `activities_round_trip`; the retry needs a fault-injection harness.
