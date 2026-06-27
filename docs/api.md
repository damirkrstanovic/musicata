# Musicata API

Musicata exposes two HTTP APIs:

- **Native API** (`/api`) — the first-party interface the web app uses. JSON over
  HTTP plus a WebSocket for live player state.
- **OpenSubsonic API** (`/rest`) — a compatibility surface so third-party
  Subsonic/OpenSubsonic clients can browse and stream the library.

All endpoints are served from the same origin/port (default `127.0.0.1:3030`).

---

## Native API (v1)

The native API is versioned as **v1**. `GET /api/health` reports the current version:

```json
{ "status": "ok", "provider": "local-disk", "tracks": 123,
  "api_version": "1", "server_version": "0.1.0" }
```

The version is bumped only on breaking changes to routes or payloads. New
fields/endpoints are additive within a version.

### Authentication

`/api/*` requires authentication once an account exists. Until the first user is
created the server runs in **setup mode** and the API is open. These paths are always
open: `/api/health`, `/api/auth/status`, `/api/auth/login`, `/api/auth/setup`.

Authenticate with any of:

- the session cookie `musicata_session` (set by `POST /api/auth/login`), or
- an API token via `?token=…`, or `Authorization: Bearer …`.

`POST /api/auth/setup` creates the first admin account (allowed only in setup mode).

### Conventions

- Responses are JSON. List endpoints return a page envelope:
  `{ "items": [...], "total": N, "limit": N, "offset": N, "sort": "..." }`.
- Errors return `{ "error": { "code": "...", "message": "..." } }` with an HTTP
  status (400 invalid request, 403 forbidden, 404 not found, 500 internal).
- IDs are opaque strings (`track_…`, `album_…`, `artist_…`).

### Accounts & auth

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/auth/status` | Whether setup is needed and who (if anyone) is signed in. (open) |
| POST | `/api/auth/setup` | Create the first admin account (setup mode only). (open) |
| POST | `/api/auth/login` | Sign in; sets the `musicata_session` cookie. (open) |
| POST | `/api/auth/logout` | Sign out; clears the session. |
| GET | `/api/auth/me` | The current account. |
| POST | `/api/auth/password` | Change the current account's password. |
| GET/POST | `/api/auth/token` | Get / rotate the current account's API token. |

### Users (admin-only)

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET/POST | `/api/users` | List users / create a user. |
| PATCH/DELETE | `/api/users/{id}` | Update / remove a user. |

### Library, browse, search

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/library/summary` | Track/album/artist counts and provider. |
| POST | `/api/library/rescan` | Force a filesystem rescan (the server also rescans on a timer). |
| GET/POST | `/api/library/export` | Export status / start a background library export. |
| GET | `/api/library/export/download` | Download the completed export archive. |
| POST | `/api/library/import` | Import a previously exported library archive. |
| GET | `/api/artists`, `/api/artists/{id}` | List artists / artist detail. |
| GET | `/api/albums`, `/api/albums/{id}` | List albums / album detail (with tracks). |
| GET | `/api/tracks` | List tracks. Filters: `genre`, `year`, `composer`, `folder`; paging: `limit`, `offset`, `sort`. |
| GET | `/api/browse` | Facets for genre/year/composer. |
| GET | `/api/browse/recently-added` | Newest tracks first. |
| GET | `/api/search?query=…` | Full-text search; returns `{ artists, albums, tracks }`. |
| GET | `/api/tracks/{id}/stream` | Stream a track (supports HTTP `Range`). |
| GET | `/api/albums/{id}/artwork` | Album cover image (ETag-cached). |

### Music sources

Every origin of music — the local library, an SMB share, internet radio — is a
*provider*. Each advertises `capabilities` (`can_scan`/`can_browse`/`can_search`/
`can_stream`); scannable sources merge into the one library and tracks keep their
`provider_id`. The local library is always present and comes from `--library`;
network sources are added at runtime and persisted.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/sources` | List sources: `{ id, kind, display_name, enabled, capabilities }`. |
| POST | `/api/sources` | Add a network source. SMB: `{ "kind":"smb", "host", "share", "base_path"?, "display_name"?, "username"?, "password"? }`. OpenSubsonic: `{ "kind":"opensubsonic", "host", "username", "password" }`. Podcast: `{ "kind":"podcast", "host":"<feed-url>", "display_name"? }` (browse-only; episodes are streams, not scanned — requires the `provider-podcast` build feature). Internet Archive: `{ "kind":"archive", "host":"<item-id-or-details-url>", "display_name"? }` (browse-only; an item's audio files are streamed — requires the `provider-archive` build feature). Builds + scans/validates it. |
| DELETE | `/api/sources/{id}` | Remove a source and re-merge the library (not the local source). |
| POST | `/api/sources/{id}/rescan` | Rescan all sources and persist the merged library. |
| GET | `/api/sources/{id}/browse` | Browse a source's hierarchy (for browse-only providers like radio). |
| GET | `/api/sources/{id}/resolve` | Resolve a browse entry to a streamable track. |

SMB shares are read directly over the wire in pure Rust (no kernel mount);
streaming fetches only the requested byte range.

### Listening history

| Method | Path | Purpose |
| ------ | ---- | ------- |
| DELETE | `/api/history` | Clear all listening history (privacy action, admin-only). Returns `{ removed }`. Recording itself is gated by the `history_enabled` setting. |
| GET | `/api/history/recent?limit=` | Distinct tracks, most-recently-played first; each has `last_listened_at`. |
| GET | `/api/history/most-played?limit=` | Tracks with a `play_count`. |
| GET | `/api/history/stats` | Aggregate listening stats: `total_plays`, `total_skips`, `distinct_tracks_played`, `plays_last_7_days`, `plays_last_30_days`, `current_streak_days`, `longest_streak_days`, `listening_sessions`, `longest_session_plays`, `favorite_tracks`/`favorite_albums`/`favorite_artists`. A session is a run of plays < 30 min apart; a streak is consecutive UTC days with a play. Counts are over the retained history window. |

### Playlists and favorites

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/playlists` | List playlists (id, name, comment, song_count, timestamps). |
| POST | `/api/playlists` | Create: `{ "name", "comment"?, "track_ids"? }`; returns the playlist with its tracks. |
| GET | `/api/playlists/{id}` | Playlist with its ordered `tracks`. |
| PATCH | `/api/playlists/{id}` | `name`/`comment` to edit; `track_ids` to replace/reorder; or `add_track_ids` + `remove_indices`. |
| DELETE | `/api/playlists/{id}` | Delete a playlist. |
| GET | `/api/favorites` | Starred `{ tracks, albums, artists }`. |
| PUT | `/api/favorites/{kind}/{id}` | Star (`kind` = `track`/`album`/`artist`). |
| DELETE | `/api/favorites/{kind}/{id}` | Unstar. |
| GET | `/api/smart-playlists` | List smart (rule-based) playlists. |
| GET | `/api/smart-playlists/{id}` | A smart playlist with its computed tracks. |

### Players and zones

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/players` | List registered players. Each carries `capabilities` (`seek`/`volume`/`repeat`/`shuffle`/`queue`) advertised by the backend. |
| POST | `/api/players` | Register a player: `{ "kind": "mpd"\|"native", "address", "name", "issue_token"?: bool }`. `native` is a self-registering endpoint (see [native-endpoint.md](native-endpoint.md)). With `issue_token`, the response also carries `auth_token` **once** — a per-player endpoint token (only its hash is stored). |
| PATCH | `/api/players/{id}` | Rename (`{"name"}`) or set zone (`{"zone_id"}`/`null`). User-gated. |
| DELETE | `/api/players/{id}` | Remove a player. User-gated. |
| GET | `/api/players/{id}/state` | Current `PlaybackState` snapshot. A player's own endpoint token authenticates this channel (Bearer/`?token=`) in place of a user session. |
| POST | `/api/players/{id}/commands` | Send a `PlayerCommand` (see below). Endpoint-token auth as for `/state`. |
| GET | `/api/players/{id}/ws` | WebSocket of live `PlaybackState`. Endpoint-token auth via `?token=`. |
| GET/POST | `/api/zones` | List zones / create a zone (named groups of players). |
| PATCH/DELETE | `/api/zones/{id}` | Rename / remove a zone. |
| GET | `/api/zones/{id}/state` | Current `PlaybackState` for the zone. |
| POST | `/api/zones/{id}/commands` | Apply a command to every player in the zone. |
| GET | `/api/zones/{id}/ws` | WebSocket of live zone `PlaybackState`. |

The local browser player is always present as id `browser-local`.

### Player commands

`POST /api/players/{id}/commands` accepts a JSON object with a `command` field:

| `command` | Extra fields |
| --------- | ------------ |
| `play`, `pause`, `stop`, `next`, `previous`, `clear` | — |
| `seek` | `position_seconds` (number) |
| `set_volume` | `volume` (0–100) |
| `set_repeat` | `mode` (`off`/`all`/`one`) |
| `set_shuffle` | `enabled` (bool) |
| `play_tracks` | `track_ids` (string[]) — replace queue and play; `start_index` (number, optional, default 0) — queue position to start at |
| `enqueue` | `track_ids` (string[]) |
| `play_queue_index` | `index` (number) |
| `remove_queue_item` | `index` (number) |
| `move_queue_item` | `from`, `to` (numbers) |

### WebSocket: `/api/players/{id}/ws`

The server pushes a full `PlaybackState` JSON message on every real change (play/
pause, track change, queue edit, volume/repeat/shuffle):

```json
{
  "status": "playing",            // stopped | playing | paused
  "now_playing": {                // null when nothing is loaded
    "track_id": "track_…", "title": "…", "artist": "…",
    "album": "…", "stream_url": "/api/tracks/track_…/stream",
    "artwork_url": "/api/albums/album_…/artwork"
  },
  "elapsed_seconds": 12.3,
  "duration_seconds": 215.0,
  "volume": 100,
  "repeat": "off",                // off | all | one
  "shuffle": false,
  "queue": [ /* QueueItem[] */ ],
  "queue_position": 0
}
```

Position is **not** sent as full state. While a track plays, the server emits a
lightweight position-only frame (for the browser player, ~1×/second) so controllers
can advance their seek bar without re-receiving the whole queue every tick:

```json
{ "type": "progress", "elapsed_seconds": 12.3, "duration_seconds": 215.0 }
```

A client distinguishes the two by the presence of a `type` field: a frame with
`type: "progress"` is a position tick (apply elapsed/duration only); any other frame
is a full `PlaybackState`.

For the browser player, the output tab also sends the same-shaped frames in the
client→server direction so the server-owned state tracks real playback (the server
re-broadcasts position to other controllers as the `progress` frame above):

```json
{ "type": "progress", "elapsed_seconds": 12.3, "duration_seconds": 215.0 }
{ "type": "ended" }
```

### Playback sessions

A browser claims audio output by opening a playback session; the heartbeat over its
event stream keeps it alive.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| POST | `/api/playback/sessions` | Open a playback session (claim browser output). |
| DELETE | `/api/playback/sessions/{id}` | Close a playback session. |
| GET | `/api/playback/sessions/{id}/events` | Session event stream / heartbeat. |

### History, autoplay, radio

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET/PUT | `/api/autoplay` | Get / set the autoplay (continuous-play) settings. |
| GET/POST | `/api/radio` | List internet-radio stations / add one. |
| GET | `/api/radio/directory` | Browse the Radio Browser directory. |
| PATCH/DELETE | `/api/radio/{id}` | Edit / remove a station. |
| GET | `/api/tracks/{id}/radio` | A track-seeded radio (similar tracks). |
| GET | `/api/tracks/{id}/similar?limit=` | "Sounds like this" — tracks whose **audio embedding** is nearest the seed (cosine KNN over the musicata-ml `vec0` index), nearest first. `{ track_ids }`. Empty if the seed hasn't been analyzed; distinct from `/radio` (which is ListenBrainz/metadata-based). |
| GET | `/api/tracks/{id}/audio-radio?limit=` | A **diverse** audio station from the seed: sonically-similar tracks **interleaved across artists** (no artist back-to-back, capped share) so it varies instead of repeating one artist. `{ track_ids }` with the seed first. The DJ version of `/similar`. |

### Identification & enrichment

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/identification/stats` | Fingerprint/identification progress counts. |
| GET | `/api/identification/unidentified` | Tracks still awaiting identification. |
| GET | `/api/artists/aliases` | List artist merge aliases. |
| DELETE | `/api/artists/aliases/{alias_key}` | Unmerge an artist alias. |
| POST | `/api/artists/merge` | Merge artists under one identity. |

### Metadata & artwork review

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/tracks/{id}/metadata/review` | The track's per-field metadata review. |
| PATCH | `/api/tracks/{id}/metadata/review/fields` | Approve/reject reviewed fields. |
| GET | `/api/tracks/{id}/metadata/musicbrainz` | MusicBrainz lookup for the track. |
| GET | `/api/tracks/{id}/metadata/musicbrainz/candidates` | MusicBrainz match candidates. |
| GET | `/api/metadata/write-back` | Tag write-back policy status (POST rejects write-back). |
| GET | `/api/albums/{id}/metadata/musicbrainz/candidates` | Album MusicBrainz candidates. |
| GET | `/api/albums/{id}/artwork/cover-art-archive/candidates` | Cover Art Archive candidates. |
| GET | `/api/albums/{id}/artwork/review` | The album's artwork review. |
| GET | `/api/albums/{id}/artwork/candidates/{artwork_id}` | One artwork candidate's bytes. |
| PATCH | `/api/albums/{id}/artwork` | Select / set the album's artwork. |
| GET | `/api/artists/{id}/artwork` | Artist image (404 until acquired). |

### Activity & settings

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/activity` | Recent background-work activity log. |
| GET | `/api/activity/ws` | WebSocket of live activity entries. |
| GET/PATCH | `/api/settings` | Read / update product settings. |

### DSP profiles

EQ + room/headphone correction profiles. Authenticated (not admin-only).

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/dsp/profiles` | List DSP profiles. |
| PUT/DELETE | `/api/dsp/profiles/{id}` | Upsert / delete a profile. |
| GET/POST/DELETE | `/api/dsp/profiles/{id}/impulse` | Get / upload (WAV) / delete a profile's impulse response. |

### Snapcast (feature-gated)

Present only with the `snapcast` build feature.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET/PATCH | `/api/snapcast/status` | Snapcast config / state. |
| GET/POST | `/api/snapcast/rooms` | List / add a room. |
| DELETE | `/api/snapcast/rooms/{name}` | Remove a room. |
| POST | `/api/snapcast/clients/{id}/volume` | Set a client's volume. |

---

## OpenSubsonic API (`/rest`)

A subset of the [OpenSubsonic](https://opensubsonic.netlify.app/) / Subsonic REST API
sufficient for a third-party client to authenticate, browse, search, fetch cover art,
and stream. Every response advertises `openSubsonic="true"` and `type="musicata"`.

### Authentication & configuration

Credentials are configured on the server:

- `--subsonic-user` / `MUSICATA_SUBSONIC_USER` / config `subsonic_user` (default `musicata`)
- `--subsonic-password` / `MUSICATA_SUBSONIC_PASSWORD` / config `subsonic_password`

Clients authenticate per request with the standard Subsonic parameters:

- `u` — username
- `p` — password, plaintext or hex (`p=enc:<hex>`)
- or `t` + `s` — token auth, where `t = md5(password + salt)` and `s` is the salt
- `v` — protocol version, `c` — client name, `f` — `xml` (default) or `json`

`/rest` now authenticates against real accounts via the user's API token (the
salted-token `t`+`s`, plaintext `p`, or hex `p=enc:<hex>` forms above are matched
against the account's token). Open / single-user fallback applies only in setup mode
(before any account exists), for trusted-LAN use; the server logs a warning at startup.

### Response format

`f=xml` (default) or `f=json`. Both wrap the payload in a `subsonic-response`:

```xml
<subsonic-response status="ok" version="1.16.1" type="musicata" openSubsonic="true">…</subsonic-response>
```
```json
{ "subsonic-response": { "status": "ok", "version": "1.16.1", "type": "musicata", "openSubsonic": true, … } }
```

Errors use `status="failed"` with `<error code="…" message="…"/>`. Codes: `10`
missing parameter, `40` wrong username/password, `70` not found.

### Supported methods

`/rest/<method>` and `/rest/<method>.view` are equivalent.

| Method | Notes |
| ------ | ----- |
| `ping` | Connectivity / auth check. |
| `getLicense` | Always valid. |
| `getOpenSubsonicExtensions` | Advertises `formPost` and `songLyrics`. |
| `getMusicFolders` | A single folder. |
| `getGenres` | Library genres with song counts. |
| `getArtists` / `getIndexes` | Artists grouped into alphabetical indexes. |
| `getArtist` | Artist with its albums (`id`). |
| `getAlbum` | Album with its songs (`id`). |
| `getSong` | One song (`id`). |
| `getMusicDirectory` | Folder-style browse: artist `id` → albums as dirs, album `id` → songs. |
| `getAlbumList` / `getAlbumList2` | Album lists; `type` (`alphabeticalByName`, `alphabeticalByArtist`, `newest`, `byYear`, …), `size`, `offset`. |
| `getRandomSongs` | `size` random songs. |
| `getSongsByGenre` | Songs in a `genre`; `count`, `offset`. |
| `search2` / `search3` | `query`; returns artists, albums, songs. |
| `getLyrics` / `getLyricsBySongId` | Stored lyrics (legacy by artist/title; OpenSubsonic structured by song `id`). |
| `getCoverArt` | Image bytes for an album or song `id`. |
| `stream` / `download` | Audio bytes for a song `id` (supports `Range`). |
| `scrobble` | Records a listen (`submission=true`) into Musicata's history. |
| `getPlaylists` / `getPlaylist` | List playlists / one playlist with entries. |
| `createPlaylist` / `updatePlaylist` / `deletePlaylist` | Create (name + `songId`s), edit (`songIdToAdd`/`songIndexToRemove`), delete. |
| `star` / `unstar` | Star/unstar by `id` (song/album/artist), `albumId`, or `artistId`. |
| `getStarred` / `getStarred2` | Starred artists, albums, and songs. |
| `getInternetRadioStations` | List internet-radio stations. |
| `createInternetRadioStation` / `updateInternetRadioStation` / `deleteInternetRadioStation` | Manage internet-radio stations. |
| `setRating` | Accepted as a no-op (ratings aren't stored). |
| `getUser` | Reports `streamRole`, `downloadRole`, and `scrobblingEnabled`. |

IDs are Musicata's own (`artist_…`, `album_…`, `track_…`); clients treat them as
opaque. `coverArt` on albums and songs is the album id.

### Limitations

- Song `duration` is read from the audio stream at scan time and reported, along with
  an approximate average `bitRate`. Libraries scanned before this was added populate
  duration on their next rescan (or a forced `--rescan`).
- No transcoding: `stream` returns the original file.
- Numeric ratings (`setRating`) and play-queue sync (`savePlayQueue`) aren't stored yet;
  shares and the jukebox are not implemented.
