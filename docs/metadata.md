# Metadata Update Strategy

Date: 2026-05-24

## Goal

Musicata should support rich metadata updates without damaging user libraries. The safest approach is database-first: update Musicata's canonical metadata and provider mappings first, then offer explicit, reversible write-back to audio files.

## Research Summary

The strongest references are MusicBrainz Picard, beets, Lidarr, AcoustID, Cover Art Archive, and Rust metadata crates.

Key lessons:

- MusicBrainz is the best canonical open metadata source for releases, recordings, artists, works, and relationships.
- Picard is the best reference for MBID tagging, AcoustID matching, scripting, cover art, and cross-format tag naming.
- beets is the best reference for safe import workflows: it can keep files in place, store corrected metadata only in its database, and avoid writing tags.
- Lidarr is useful for long-running library management: profiles, rescans, metadata refreshes, retag events, and file-change watching.
- AcoustID/Chromaprint is useful when filenames and tags are poor, but it should create match candidates, not blindly overwrite metadata.
- Cover Art Archive is the preferred source for MusicBrainz-linked release and release-group artwork.
- Lofty is the likely first Rust crate for reading and writing common audio metadata formats.

Sources:

- https://picard.musicbrainz.org/
- https://picard-docs.musicbrainz.org/en/latest/variables/tags_basic.html
- https://picard-docs.musicbrainz.org/en/latest/appendices/tag_mapping.html
- https://docs.beets.io/en/latest/guides/main.html
- https://docs.beets.io/en/latest/plugins/index.html
- https://wiki.servarr.com/lidarr/settings
- https://musicbrainz.org/doc/MusicBrainz_API
- https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting
- https://acoustid.org/webservice
- https://musicbrainz.org/doc/Cover_Art_Archive/API
- https://docs.rs/lofty/latest/lofty/
- https://lrclib.net/docs

## Core Principle

Do not make tag write-back automatic by default.

Musicata should have three metadata layers:

1. **Observed metadata**: what was read from files or provider APIs.
2. **Canonical metadata**: Musicata's chosen library view after matching, enrichment, and user edits.
3. **Write-back metadata**: a selected subset of canonical fields that the user explicitly chooses to write to files.

This keeps the server useful for read-only libraries, network shares, streaming providers, and cautious users.

## Metadata Sources

Recommended source priority:

1. User edits inside Musicata.
2. Existing embedded MusicBrainz IDs.
3. Explicit user-selected MusicBrainz match.
4. MusicBrainz lookup by release/recording MBID.
5. AcoustID/Chromaprint match candidates.
6. Embedded file tags.
7. Folder and filename inference.
8. Optional sources such as Discogs, Spotify, Deezer, Last.fm tags, LRCLIB lyrics, fanart.tv, or provider metadata.

Every imported value should keep provenance:

- source name
- source entity ID
- confidence score
- timestamp
- raw original value
- normalized value
- whether the value was user-approved

## Matching Workflow

### First Scan

Read:

- path and filesystem metadata;
- audio format, duration, sample rate, bitrate, channels, bit depth;
- embedded tags;
- embedded MusicBrainz IDs;
- embedded artwork;
- adjacent artwork files;
- adjacent lyrics files.

Store this in the database without modifying files.

### Candidate Generation

Generate metadata candidates in this order:

1. Use embedded MBIDs when present.
2. Group likely albums by folder, album tag, album artist, disc number, and track count.
3. Search MusicBrainz by artist, album, track names, duration, and year.
4. Use AcoustID only when confidence is low or tags are missing.
5. Fetch release/release-group artwork from Cover Art Archive.
6. Fetch lyrics from local files first, then optional LRCLIB lookup.

MusicBrainz API usage must include a meaningful User-Agent and respect rate limits. Cache all lookups.

### Review And Apply

The UI should show a diff before applying:

- current file tag value;
- Musicata canonical value;
- proposed external value;
- source and confidence;
- write-back impact.

Bulk updates should support:

- fill missing only;
- overwrite all except user-edited fields;
- overwrite selected fields;
- never overwrite artwork;
- never overwrite genres;
- preserve original date vs release date preference;
- standardized artist names vs credited names.

## Write-Back Policy

Write-back should be opt-in per library and per operation.

Default behavior:

- Update Musicata database: yes.
- Write tags to audio files: no.
- Rename/move files: no.
- Delete existing tags: no.
- Replace artwork: no.

When enabled:

- Create a metadata snapshot before writing.
- Preserve file modified time where possible.
- Write only selected fields.
- Preserve unknown/private frames unless user chooses a scrub operation.
- Write to a temporary file or use the tag library's safest available write mode.
- Report per-file success/failure.
- Allow rollback from stored previous values where practical.

Do not write tags for read-only, remote, or provider-backed sources that do not declare write support.

Current implementation:

- `GET /api/metadata/write-back` returns the disabled write-back policy.
- `POST /api/metadata/write-back` returns `403 write_back_disabled`.
- Metadata review, artwork selection, and enrichment update only Musicata's database.
- **Automatic MusicBrainz enrichment** (the AcoustID fingerprint → MusicBrainz pipeline):
  for tracks whose recording MBID fingerprinting resolved, `musicbrainz_enrich_pass`
  fetches the real title/artist/album/track number from MusicBrainz and applies them to
  the canonical library (re-deriving the artist/album entities so grouping stays
  consistent — `regroup_library_with_overrides` in `musicata-core`). This is **DB-only and
  never overwrites an embedded tag** — a field is filled only when the file carried no
  embedded value for it (it was empty or folder/filename-derived). The original
  folder/embedded observations are retained, so the change is reversible and a future
  review/override + opt-in write-back can build on it. Toggle in `/admin` (default on).

Future opt-in write-back must require all of the following: provider-declared write support, per-library configuration, a per-operation preview diff, selected fields only, and a metadata snapshot that can support rollback.

## Tag Format Strategy

Use Picard-compatible tag names and MBID fields where practical so files remain useful outside Musicata.

Initial write support should target:

- FLAC/Ogg/Opus Vorbis comments.
- MP3 ID3v2.
- M4A/MP4 iTunes-style atoms.

Defer or read-only initially:

- WAV metadata write-back.
- AIFF metadata write-back.
- APE/WavPack.
- format-specific private fields.

Rust tooling:

- `lofty`: first choice for unified read/write across common formats.
- `id3`: fallback or specialized MP3 writer.
- `metaflac`: fallback or specialized FLAC writer.
- `mp4ameta`: fallback or specialized M4A/MP4 writer.

Lofty currently advertises parsing, conversion, and writing across common audio metadata formats, including MP3, FLAC, MP4, Opus, Ogg Vorbis, WAV, and others.

## Field Model

Canonical entities should store at least:

- artist, album artist, track artist, composer, conductor, lyricist;
- album, release group, recording, work;
- title, subtitle, version, movement, track number, disc number;
- release date, original date, year, country, status, barcode, catalog number, label;
- genres, styles, moods, tags;
- MusicBrainz artist/release/release-group/recording/work IDs;
- ISRC;
- AcoustID;
- artwork references;
- plain and synced lyrics;
- replay gain and audio properties.

Store multi-value fields as arrays/relations, not delimiter-joined strings. Convert to format-specific delimiters only at write-back time.

## Artwork

Artwork priority:

1. User-selected artwork.
2. Embedded artwork.
3. Local `cover`, `folder`, or `front` image.
4. Cover Art Archive release front art.
5. Cover Art Archive release-group front art.
6. Provider artwork.

Store artwork as assets with source, dimensions, MIME type, hash, and relation to album/release/release group. Do not embed new artwork into files unless the user explicitly chooses write-back.

Current implementation: Musicata reviews local artwork files, lets the user select an album candidate in the web controller, serves selected artwork with asset-keyed URLs plus HTTP cache validators, and fetches remote candidates on demand for MusicBrainz-linked albums from **four providers in priority order — Cover Art Archive, fanart.tv, iTunes, Deezer** (`artwork_providers.rs`: `CoverArtArchiveProvider`, `FanartTvProvider`, `ItunesProvider`, `DeezerProvider`). Automatically acquired remote covers are **downloaded and cached as local assets** (`acquired_cache_key` + the artwork cache; `upsert_acquired_artwork` / `reapply_acquired_artwork` survive rescans). Selecting the current automatic cover pins its bytes as an explicit choice. Embedded artwork is extracted through the source provider when no folder cover is available.

### Where covers are stored on disk

Musicata splits cover art into two cases:

- **Fetched / acquired covers** (from the artwork-provider lane — iTunes, Deezer, Cover Art
  Archive, fanart.tv). The image **bytes are cached as files on disk**, next to the database in
  a sharded cache-key layout:

  ```
  .musicata/artwork/<first-2-chars-of-key>/<cache_key>.<ext>  # source alias
  .musicata/artwork/objects/<sha256>                       # deduplicated bytes
  .musicata/artwork/pinned/<first-2-chars>/<key>.<ext>       # manual-choice alias
  ```

  The cache directory is derived from the database path (`<db parent>/artwork/`). The database
  stores only **provenance**, not the bytes — the `acquired_album_artwork` table records
  `album_id, provider, remote_url, cache_key, ext, width, status, acquired_at`, and `cache_key`
  maps a row to its file on disk. The table is intentionally foreign-key-less so a rescan's
  album rewrite can't wipe it; acquired covers are re-pointed back onto their albums after every
  scan.

- **Local / embedded covers** (a folder `cover.jpg`, or art embedded in the audio tags). These
  retain their source reference in `albums.artwork_path` (the audio file itself for embedded
  art). Folder-image bytes and extracted embedded images are cached on demand in the same
  `.musicata/artwork/` directory, alongside their thumbnails. Audio files are never copied into
  the artwork cache.

Embedded extraction uses a seekable local/SMB reader and a 16 MiB cumulative metadata
read budget, rather than buffering the recording. Large audio files remain supported;
oversized or malformed tag data is rejected. Local image and cache reads have a 32 MiB
byte limit, matching the existing SMB cover limit; downloaded images retain their
16 MiB limit and image decoders retain their independent allocation limit. Source
misses and decoding share three processing permits across requests and prewarming,
held through blocking extraction and resizing even when a request is cancelled.
Cached responses and 304s do not wait for source-processing permits. These limits bound artwork-processing
buffers; they are not a whole-process memory ceiling. Cache objects carry their SHA-256 identity;
changed/truncated objects are rejected and reacquired through their recorded source. Legacy
entries are validated with bounded decoding before lazy migration, with backoff on migration
failure. Successful cache hits do not decode images again.

Source artwork uses revision metadata from the existing directory walk: provider identity,
logical source path, file size and modification time, plus an available audio content hash
for embedded artwork. The folder-image entries are shared between tracks rather than listed
again or inspected as local paths for SMB sources. An unchanged audio file reuses its parsed
metadata while still noticing added, removed or changed neighboring covers. These source
revisions survive regrouping and are carried in the existing artwork asset URL. Album-level
cover choices are retained across disc directories while the chosen file is still present.

Periodic scans, manual/forced rescans and `--scan-once` all refresh source artwork.
Manual rescans reuse the prior catalogue's unchanged tags and cover choice; forcing still
requests a database rewrite. After a scan, artwork-only changes update only album artwork fields and invalidate stale
`local-cache` provenance; they do not rewrite tracks or the whole library. Repeated unchanged
scans do not write album artwork. External acquisitions keep precedence, and a warmer finishing
late can promote a local cover only if its revision is still current. Cached requests use the
persisted revision without stat/SMB calls and remain usable with unavailable sources.

Source revision discovery remains metadata-based: changing image contents while preserving
both size and reported modification time requires **Refresh artwork** or a source revision change.
Cached originals and thumbnails are deduplicated by SHA-256 of their bytes. Thumbnail aliases use
the original image's content hash, so an old resize finishing after Refresh cannot restore a stale
thumbnail for new image contents. Source aliases retain provenance separately. `artwork_provider_id`
records the source that supplied the selected cover, including albums merged across providers.

**Manual controls.** Album detail → Change artwork offers local folder candidates, the current
automatic cover (resolved through its provider, including an offline cache hit), and image upload.
Administrators can select/replace, refresh, or clear. Uploads accept JPEG/PNG/GIF/WebP up to
16 MiB and validate the independent 64 MiB decode allocation limit before storing anything.
A choice is copied into managed storage before its DB reference is published; it survives source
removal, scans, restarts, and identity-reference remapping. Manual choices take precedence over
automatic acquisitions. Clear removes the manual choice and returns to the current automatic
cover. Refresh clears automatic cache/retry state and advances a persisted browser-cache generation;
it preserves the manual choice. Network paths are never opened as local candidate paths. The
network-source review offers the current automatic cover and upload, not a recursive remote
folder-image browser. All image URLs remain same-origin.

**Retry state.** Successful absence is durable for the source revision. I/O, parser and decode
failures retry after 5 minutes, doubling up to 6 hours, across server restarts. Revision changes
retry immediately; Refresh explicitly resets the deadline. Requests and prewarming serialize
misses per album. No source work is coupled to playback controls.

**Cache budget.** Settings → Metadata & artwork exposes a live 512 MiB default automatic-cache
budget (16–65536 MiB). An independent maintenance worker runs every minute. It counts content
objects, aliases and legacy files, evicts least-recently-used automatic objects/aliases, and
coalesces access timestamps to disk for restart recovery. It scans outside mutation locks; cached
GETs do not wait for maintenance. The budget is eventual: writes between passes can temporarily
exceed it. Explicitly chosen originals are user assets outside this budget; their thumbnails remain
disposable. Unreferenced pins have a one-hour grace period before cleanup. Missing automatic
entries are reacquired from their provider on demand. Prewarming remains a separate bounded worker.

Audio fingerprinting (AcoustID) feeds the first case: once an untagged track resolves to
MusicBrainz IDs, the id-exact providers (Cover Art Archive / fanart.tv) can download its cover
into that cache. Inspect it with:

```sh
ls .musicata/artwork/
sqlite3 .musicata/musicata.db "select album_id, provider, ext, status from acquired_album_artwork limit 10"
```

## Lyrics

Lyrics priority:

1. Embedded synced lyrics.
2. Adjacent `.lrc` file.
3. Embedded plain lyrics.
4. Adjacent `.txt` lyrics.
5. Optional LRCLIB lookup.

Current implementation: Musicata stores embedded Lofty lyrics and adjacent same-stem `.lrc` or `.txt` sidecar lyrics as observed metadata. Sidecar `.lrc` files are preferred over `.txt` files when both exist. LRCLIB lookup remains future work.

LRCLIB can fetch plain and synchronized lyrics by track title, artist, album, and duration. Its docs recommend a User-Agent even though no API key is required.

## User Experience

Metadata should have dedicated UI:

- metadata health dashboard;
- unmatched albums;
- conflicting candidates;
- missing artwork;
- missing MBIDs;
- duplicate recordings;
- bulk edit preview;
- write-back queue;
- history/rollback view.

Avoid a one-click "fix everything" workflow. Music libraries contain personal choices, rare releases, unofficial files, bootlegs, custom genres, DJ edits, and provider-specific versions.

## Implementation Sequence

1. Read embedded tags with Lofty.
2. Store raw observed metadata and canonical metadata separately.
3. Add metadata provenance and per-field source tracking.
4. Add MusicBrainz lookup by existing MBIDs.
5. Add MusicBrainz candidate search for unmatched albums.
6. Add Cover Art Archive artwork enrichment behind explicit review.
7. Add sidecar `.lrc` and `.txt` lyric observations.
8. Add user review/apply UI.
9. Add optional tag write-back for FLAC, MP3, and M4A.
10. Add AcoustID/Chromaprint matching.
11. Add LRCLIB lyrics enrichment.
12. Add advanced metadata sources and automatic refresh policies.

## Recommendation

For Musicata, the best first implementation is **safe enrichment, not retagging**:

- make the database view rich and correct;
- preserve all original file tags;
- allow user-approved corrections;
- write to files only when explicitly enabled;
- keep a revision trail.

This is the best fit for a Roon-like server because playback, browsing, and recommendations can use enriched metadata immediately without forcing destructive changes onto the user's music files.
