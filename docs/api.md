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

### Conventions

- Responses are JSON. List endpoints return a page envelope:
  `{ "items": [...], "total": N, "limit": N, "offset": N, "sort": "..." }`.
- Errors return `{ "error": { "code": "...", "message": "..." } }` with an HTTP
  status (400 invalid request, 403 forbidden, 404 not found, 500 internal).
- IDs are opaque strings (`track_…`, `album_…`, `artist_…`).

### Library, browse, search

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/library/summary` | Track/album/artist counts and provider. |
| POST | `/api/library/rescan` | Force a filesystem rescan (the server also rescans on a timer). |
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
| POST | `/api/sources` | Add a network source. SMB: `{ "kind":"smb", "host", "share", "base_path"?, "display_name"?, "username"?, "password"? }`. Builds + scans it. Requires the `provider-smb` build feature. |
| DELETE | `/api/sources/{id}` | Remove a source and re-merge the library (not the local source). |
| POST | `/api/sources/{id}/rescan` | Rescan all sources and persist the merged library. |

SMB shares are read directly over the wire in pure Rust (no kernel mount);
streaming fetches only the requested byte range.

### Listening history

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/history/recent?limit=` | Distinct tracks, most-recently-played first; each has `last_listened_at`. |
| GET | `/api/history/most-played?limit=` | Tracks with a `play_count`. |

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

### Players and zones

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/players` | List registered players. |
| POST | `/api/players` | Register a player: `{ "kind": "mpd", "address": "host:port", "name": "…" }`. |
| PATCH | `/api/players/{id}` | Rename (`{"name"}`) or set zone (`{"zone_id"}`/`null`). |
| DELETE | `/api/players/{id}` | Remove a player. |
| GET | `/api/players/{id}/state` | Current `PlaybackState` snapshot. |
| POST | `/api/players/{id}/commands` | Send a `PlayerCommand` (see below). |
| GET | `/api/players/{id}/ws` | WebSocket of live `PlaybackState`. |
| GET/POST | `/api/zones`, `/api/zones/{id}` | Manage zones (named groups of players). |
| POST | `/api/zones/{id}/commands` | Apply a command to every player in the zone. |

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

If no `subsonic_password` is configured the API runs **open** (any credentials
accepted) for trusted-LAN use; the server logs a warning at startup. Real user
authentication and a network security model are Milestone 12.

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
| `getUser` | Reports stream/download/scrobble roles. |

IDs are Musicata's own (`artist_…`, `album_…`, `track_…`); clients treat them as
opaque. `coverArt` on albums and songs is the album id.

### Limitations

- Song `duration` is read from the audio stream at scan time and reported, along with
  an approximate average `bitRate`. Libraries scanned before this was added populate
  duration on their next rescan (or a forced `--rescan`).
- No transcoding: `stream` returns the original file.
- Numeric ratings (`setRating`) and play-queue sync (`savePlayQueue`) aren't stored yet;
  internet radio, shares, and the jukebox are not implemented.
- Starring/ratings/playlists are not persisted yet.
