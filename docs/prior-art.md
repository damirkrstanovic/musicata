# Prior art — how other music servers solve our problems

A living knowledge base: for each hard problem Musicata hits, how the reference
projects (Roon, Jellyfin, Navidrome, beets, Picard, Music Assistant, Mopidy) solve
it, and what we adopted — with pointers to our code. Covers: provider/plugin
ecosystem, incremental scanning, OpenSubsonic, SMB access, background-work UX,
metadata sourcing & conflict resolution, and discography completeness ("what am I
missing"). When you tackle one of these areas, read the
relevant section first instead of re-deriving it. When you learn something new from
another project, add it here.

Checkouts referenced during research: Jellyfin at `../jellyfin`,
Navidrome at `../navidrome`.

---

## 1. Provider / plugin ecosystem (music sources + player endpoints)

**Problem:** support many origins of music (local disk, network share, streaming
services) and many outputs (browser, MPD, network players) behind one API, so
adding one is cheap and sources can differ in what they support.

**How others solve it:**
- **Roon** splits **Core** (library/metadata/queue/DSP — authoritative), **Control**
  (thin remotes), **Output** (endpoints). Two extensibility surfaces: **RAAT** (audio
  transport, device owns the clock, closed partner SDK) and the **Extension SDK**
  (control-plane only, out-of-process Node over WebSocket RPC). Music sources are
  closed/first-party. Rules worth taking: Core authoritative; capability negotiation;
  **source layer ≠ transport layer**; explicit auth/pairing; tier endpoints
  *native* (deep, synced) vs *bridged* (just-works).
- **Music Assistant** (closest blueprint): `MusicProvider` + `PlayerProvider` classes;
  `supported_features: [ProviderFeature]`; two-phase playback
  `get_stream_details()` → `get_audio_stream()`; providers compiled into the server.
- **Mopidy**: a backend exposes `LibraryProvider` + `PlaybackProvider` +
  `PlaylistsProvider`; in-process.
- **Navidrome**: sandboxed **WASM** plugins (for untrusted third-party code).
  **Jellyfin**: out-of-process C# assembly DLLs.

**What Musicata does:** in-process, compiled-in providers via **enum dispatch**
(not `dyn`) so async methods stay object-safe — a new source/player is one enum
variant + match arms, gated by a cargo feature. `ProviderCapabilities`
(`can_scan/browse/search/stream`) is the capability-negotiation analog. A `SourceFs`
VFS lets one scanner serve any backend. See **`docs/plugins.md`** for the full Roon
research + plan; code in `crates/musicata-server/src/providers.rs`,
`crates/musicata-core/src/lib.rs` (`MusicProvider`, `SourceFs`), and the
`PlayerHandle` enum in `players.rs`. Deferred (documented in plugins.md): WASM/
subprocess host, bridged endpoints (Snapcast/AirPlay), server↔player auth.

---

## 2. Incremental library scanning (don't re-read everything)

**Problem:** picking up new/changed/removed files cheaply — re-reading every file's
tags every pass is brutal over a network share.

**How Jellyfin solves it** (the model we copied):
- **Change detection by mtime.** Stores each item's `DateModified` (file
  `LastWriteTimeUtc`); `BaseItem.RequiresRefresh()` compares it to disk, >1s delta =
  changed (`MediaBrowser.Controller/Entities/BaseItem.cs`, `BaseItemExtensions.HasChanged`).
- **Skip metadata re-reads** for unchanged items via `MetadataRefreshMode`
  (None/ValidationOnly/Default/FullRefresh) — providers only run on first-see, changed,
  or explicit full refresh (`MediaBrowser.Providers/Manager/MetadataService.cs`).
- **`DirectoryService` caches** `GetFileSystemEntries(path)` once per scan session, so
  each directory is stat'd once (`MediaBrowser.Controller/Providers/DirectoryService.cs`).
- **Real-time `FileSystemWatcher`** (recursive) + a `FileRefresher` that **debounces**
  (~5s) and coalesces bursts into a targeted refresh of just the affected path
  (`Emby.Server.Implementations/IO/LibraryMonitor.cs`, `FileRefresher.cs`).
- **Periodic full scan is only a safety net — every 12 hours**
  (`ScheduledTasks/Tasks/RefreshMediaLibraryTask.cs`), catching offline/missed changes.

**What Musicata does:** `scan_source_incremental(fs, id, prior, progress)` in
`crates/musicata-core/src/lib.rs` — the walk lists each dir once (one SMB round-trip,
entries carry size+mtime); a file whose relative path + **size + mtime** match the
prior library reuses its parsed track wholesale (no tag read, no cover-art listing);
only new/changed files are opened. Both source kinds now have a **watcher**, so the
periodic pass is just a safety net (Jellyfin's model): a `notify` recursive watcher on the
local root (`spawn_library_watcher`, 2s debounce) for on-disk libraries, and **SMB2
change-notify** for network shares (`SmbProvider::watch_changes` via the `smb` crate's
`Directory::watch_timeout`, recursive; `spawn_smb_watchers` debounces + triggers an
incremental rescan, reconnects on drop with a catch-up scan). With both in place
`LIBRARY_RESCAN_INTERVAL` is an **infrequent safety net (1h)**, not the primary path — we no
longer re-walk the whole SMB tree every minute (which was constant NAS load). `detect_library_changes`
only decides whether to *write*. (Earlier note, now resolved: SMB *does* have a watcher —
SMB2 `CHANGE_NOTIFY` — so we no longer need a tight poll to stand in for one.)

---

## 3. OpenSubsonic API compatibility

**Problem:** be compatible with the wide range of Subsonic/OpenSubsonic clients.

**How Navidrome does it:** Navidrome is the de-facto reference OpenSubsonic server
(`../navidrome`, `server/subsonic/`). It implements the full surface, the
OpenSubsonic extensions (e.g. `songLyrics`, transcode offsets, `formPost`), reads
params from query string **and** POST form body, and supports the legacy token
(`t`+`s` MD5 salt) plus plaintext/`enc:` auth.

**What Musicata does:** `crates/musicata-server/src/subsonic.rs` — one
`serde_json::Value` per endpoint rendered to **both XML and JSON** via a generic
`write_element` (scalars→attributes, arrays→repeated elements, a `value` key→element
text). Auth supports plaintext `p`, hex `p=enc:`, and the legacy `t`+`s` token, plus
open mode when no password. Params are read from query **and** form body (clients
POST). Validated against two real client libs (py-sonic JSON, go-subsonic XML) and
compared for completeness against Navidrome. See `docs/api.md` for the endpoint
table and known gaps (ratings, play-queue sync).

---

## 4. Network share access without mounting

**Problem:** read music off an SMB/CIFS share without requiring a host-level mount.

**How others do it:** Jellyfin/Navidrome generally rely on the OS mounting the share
(a path on disk); they don't speak SMB themselves.

**What Musicata does (divergence):** read the share **directly over the wire in pure
Rust** via the `smb` crate (no kernel mount, no libsmbclient/FFI), feature-gated
`provider-smb`. `crates/musicata-server/src/smb.rs`: an `SmbFs` implements the
`SourceFs` VFS so the shared scanner walks it; a `Read+Seek`-over-`read_at` adapter
with a read-ahead block cache feeds lofty (its many small seeks would otherwise be
one round-trip each); streaming fetches only the requested byte range; guest/
anonymous needs `allow_unsigned_guest_access` + a non-empty (`guest`) username, and
connects have a timeout so an unreachable host can't hang the request.

---

## 5. Background work: progress, errors, real-time UI

**Problem:** show long-running work (scans) with live progress and clear errors, and
not hammer the API with polling.

**How Jellyfin does it:** scheduled-task framework with progress reporting; the
`LibraryMonitor` drives real-time refresh; the web client gets updates without
re-polling everything.

**What Musicata does:** an in-memory `ActivityLog` (`activity.rs`) records each scan
(running → ok/error/interrupted) with per-source progress and root-cause messages;
it's **persisted** (migration v16, `activities` table) so history survives restart,
and a job left "running" at startup is marked **interrupted**. The admin page gets
updates over a **WebSocket** (`/api/activity/ws`, push on change via a watch channel)
rather than polling. Long work (scans, SMB connects) always runs in the background;
add-source validates connectivity synchronously and returns the root cause, then
scans in the background.

---

## 6. Metadata sourcing & conflict resolution

**Problem:** values for the same field come from several places (embedded tags,
folder/filename, MusicBrainz, user edits). What wins, when do we hit the network,
and how do we not clobber the user's corrections on the next scan?

**How others solve it:**

- **Jellyfin** (`MediaBrowser.Providers/Manager/MetadataService.cs`,
  `MediaInfo/AudioFileProber.cs`, `Plugins/MusicBrainz/`):
  - Provider order: **local reader first** (embedded tags via ATL + ffprobe filename
    fallback), then remote fetchers (MusicBrainz, then AudioDB), ordered by config
    (`LocalMetadataReaderOrder` / `MetadataFetcherOrder`).
  - **Merge rule = empty-field-only unless full refresh.** `MergeBaseItemData`:
    scalar fields are overwritten only if `replaceData || target field is empty`;
    arrays (genres/artists) append + dedupe; provider IDs use `TryAdd` (**first ID
    wins**). So in normal refresh a populated value is *not* overwritten by a
    lower-priority provider.
  - **Locked fields** (`BaseItem.LockedFields`, `IsLocked`) are skipped during merge
    and block remote fetches entirely — this is how user edits survive refreshes.
  - **MusicBrainz matching is ID-first**: use the MBID from tags → exact lookup;
    else search by name + artist MBID, then name + artist name; **takes the first
    result, no scoring**. No AcoustID. No per-field provenance (only locked/not).
- **Navidrome** (tag-first, read-only):
  - **Never modifies your files** — metadata lives in the tags; you fix tags with an
    external tool (Picard). A safe, opinionated default.
  - A `mappings.yaml` normalizes tag frames (ID3 `TPE1`, Vorbis `ARTIST`, MP4 `©nam`,
    …) to internal keys, lowercases keys, and splits multi-valued tags on delimiters.
  - **Stable identity from MusicBrainz IDs**: track persistent id derives from
    `musicbrainz_trackid` (else album+disc+track).
  - **Agents** enrich *artist/album* info + images only (not core track tags), tried
    in **priority order** (default `deezer,lastfm,listenbrainz`); first hit wins,
    fall through on miss. ListenBrainz needs MBIDs in tags.
- **beets** autotagger (the matching gold standard):
  - Clusters files by **directory** into album candidates.
  - Queries MusicBrainz by existing **MBID** / text; optional **AcoustID** fingerprint
    via the `chroma` plugin when tags are weak.
  - Computes a **distance** (0=perfect … 1=worst) summing weighted **per-field
    penalties**, plus a `data_source_mismatch_penalty`. **Recommendation tiers**
    (strong/medium/none) via thresholds (`strong_rec_thresh` default 0.04): a strong
    match **auto-applies**, weaker ones **prompt the user**.
  - Treats **MusicBrainz as canonical** and *rewrites* tags from it (opposite of
    Navidrome's read-only stance).
- **MusicBrainz Picard** — the canonical tagger; the strongest matcher of the lot,
  worth understanding in depth:
  - **Three-step workflow.** **Cluster** groups loose files into album clusters by
    their existing album/artist tags. **Lookup** searches MusicBrainz with the
    cluster's tags and matches the whole cluster to a *release*. **Scan** computes an
    **AcoustID acoustic fingerprint** (Chromaprint) per file and asks acoustid.org
    which *recording(s)* that audio is — so it identifies tracks **even with no or
    wrong tags**. Files with embedded MBIDs (`musicbrainz_albumid` /
    `musicbrainz_recordingid`) short-circuit straight to the exact release/recording.
  - **Weighted similarity scoring** is the core. Picard compares a file's metadata to
    a candidate with a weighted average of per-field similarities — title, artist,
    album, track/total numbers, **length** (numeric closeness, a few seconds'
    tolerance), release type/country/date — text fields by an edit-distance ratio.
    It surfaces match quality as **green > yellow > orange > red**.
  - **Three configurable thresholds** gate that score
    (`config/options_matching.html`): **cluster→release** (how similar a whole cluster
    must be to accept a release), **file→recording** (below it, a recording candidate
    is ignored entirely — used by Lookup/Scan of single files), and **file→track**
    (which track on a chosen release a file is assigned to; below it the file goes to
    an "unmatched files" bucket). Lower the file→track threshold when files have poor
    tags.
  - **AcoustID is the killer feature**: a crowd-sourced fingerprint→recording map, so
    audio identity doesn't depend on tags at all — the thing tag-only matchers
    (Jellyfin) can't do.
  - **Full MB data model**: release vs recording vs **work** vs release-group,
    multiple/featured artists, relationships, disc/medium structure — not just flat
    tags.
  - **Scriptable**: a Tagger Script language (`%title%` vars; `_hidden` vars not
    written to files; multi-values joined with "; ") plus separate **naming scripts**
    for file/folder layout, and Python **plugins**. Per-format **tag mapping** writes
    the right frame per container.
  - **Canonical-source-wins, opt-in write**: Save **rewrites** the file's tags from
    MusicBrainz (it *is* a tagger), but only the fields it manages and only when the
    user clicks Save; it preserves tags it doesn't own.
  - **Why it's strong:** acoustic identity (AcoustID) + a tuned weighted matcher with
    explicit thresholds + the full relational MB model + scriptable output + human-in-
    the-loop correction. Weaknesses: it's a desktop app, matching/length/scoring
    weights aren't exposed as docs (they live in the source), and good results lean on
    user review.

**Cross-project consensus worth adopting:**
1. **ID-first** — an embedded MusicBrainz ID means an exact lookup, no fuzzy search
   and no network guesswork (Jellyfin, Picard, beets all do this).
2. **Score text matches and gate auto-apply** (beets) — only auto-accept a
   high-confidence match; otherwise keep it as a review candidate.
3. **User edits / approvals are top priority and survive rescans** (Jellyfin locked
   fields).
4. **Per-field, highest-confidence-wins merge; don't overwrite a populated/approved
   field with a lower-priority source** (Jellyfin empty-field-only).
5. **Read-only to files by default** (Navidrome) — enrich the DB, don't rewrite tags
   unless the user opts in.
6. **AcoustID only as a last resort** when tags are missing/ambiguous (beets/Picard
   Scan).

**What Musicata does today** (`crates/musicata-core/src/lib.rs`,
`crates/musicata-server/src/musicbrainz.rs`): each track keeps **observations** with
`source` + `confidence` + `approval_state` (Observed/Approved/Rejected). Confidences:
`embedded_tag` 0.95, `sidecar_lrc` 0.90, `sidecar_txt` 0.80, `folder_path` 0.55.
The effective value (`canonical_metadata`) is currently **embedded → folder** per
field (lyrics: `.lrc` > `.txt`); embedded MBIDs are read and carried. MusicBrainz +
Cover Art Archive are **review-only candidates** with an approval model, not yet
auto-resolvers. We are already **read-only to files** (write-back disabled), matching
Navidrome (consensus #5).

**Gaps to close** (maps onto the model we already have — the observation's
`confidence` + `approval_state` are exactly the hooks): a **confidence/approval-ranked
per-field resolver** (so `Approved` user/MB values outrank embedded, embedded outranks
folder — consensus #3, #4); **MBID-first auto-resolution** + a **weighted-similarity
match with score thresholds** before auto-applying MB data — Picard's
cluster→release / file→recording / file→track threshold model is the template, and
its scoring (title/artist/album/tracknumber + **length** tolerance) is what to copy
(consensus #1, #2); a **tag-mapping/multi-value normalization** layer (Navidrome
`mappings.yaml`); and **AcoustID/Chromaprint** as the last-resort matcher that
identifies audio when tags are missing/wrong — the one capability tag-only matchers
lack, and the main reason Picard is strong (consensus #6). The designed ladder is in
`docs/metadata.md` (§Metadata Sources); this section is the *why* behind it.

---

## 7. Discography completeness — "what am I missing?"

**Problem:** the user owns 5 Pink Floyd albums; show them the artist's *full*
discography so they can see what they don't have and decide what to get next — i.e.
merge the **local library** with a **remote catalogue** of everything the artist
released, and diff.

**How others solve it:**

- **Lidarr** (and its predecessor **Headphones**) — the canonical product for exactly
  this. You **add/monitor an artist**; Lidarr pulls the artist's **full discography
  from MusicBrainz** and shows owned vs **Wanted → Missing**. The unit of "an album"
  is the **MusicBrainz release group**. A **Metadata Profile** filters which
  release-group types count (Album/Single/EP/Live/Compilation/Remix/Soundtrack/…) —
  the default is *studio albums only*, which is the key trick to avoid drowning the
  view in singles/live/comps. New releases appear on a periodic metadata refresh.
  (Lidarr then *acquires* them; that's its job, not necessarily ours.)
- **beets `missing` plugin** — the CLI version of the computation. `beet missing`
  lists, per album, the **missing tracks** (vs the MB tracklist); `-a` lists **missing
  albums per artist** (MusicBrainz only); `-c` counts, `-t` totals; exposes a
  `$missing` template field. Counting is free; naming the missing items costs one MB
  call. Good model for "completeness as a computed, cached field."
- **Roon** — the **local + streaming merge** done well. With TIDAL/Qobuz connected, an
  artist page shows the **whole discography in one place**: albums **"in my library"**
  (added with **+**, editable, play-counted) vs **"outside my library"** (available to
  stream, unedited), and you add an outside album to your library with **+**. It also
  unifies **versions/editions** of the same album (local + streaming) and lets you pick
  a preferred one. The model to copy: *one artist page that unifies owned + available +
  merely-known, visually distinguished, with an "add" affordance.*
- **Discogs** — the **collection + wantlist** model, and the richest release data. An
  artist page lists the discography grouped by **master release** (a master groups all
  **versions/pressings**). The user's **Collection** = what they own; the **Wantlist**
  = what they want to acquire (literally "missing pieces"); release **stats** show
  community haves/wants. The API exposes artist releases, master versions, collection
  and wantlist. Discogs is **physical/edition-oriented** (every pressing), so it's the
  better source for "which specific pressing," where MusicBrainz release groups are
  better for "which album."

**The shared data model:** an artist's discography is a list of **release groups**
(MusicBrainz) / **masters** (Discogs); "do I own this album?" is a match at
release-group granularity; completeness = owned ÷ (discography filtered by type).
Both MB and Discogs key off an **artist ID** we can get from embedded tags
(`musicbrainz_artistid`) or a name lookup.

**What Musicata could do** (no acquisition — we're the Roon/Discogs "see & decide"
side, not the Lidarr "go get it" side): we already read MusicBrainz artist/release
IDs from tags (`musicbrainz.rs`) and have an artist detail endpoint
(`/api/artists/{id}`). Add a **discography view**: resolve the artist's MBID → fetch
their **release groups** → match each to a local album (by MBID, else name+year) →
render the full list with **owned highlighted and missing greyed**, filtered by
release-group type (Lidarr's Metadata-Profile idea) so studio albums lead and
singles/live/comps are opt-in. Surface a **completeness %** per artist and an optional
**wantlist** (Discogs's model) of what to get next. It's **read-only enrichment**,
cached (one MB/Discogs call per artist, refreshed periodically — beets's "counting is
free, naming costs a call" applies). Caveats: needs the artist MBID (tag or lookup);
release-group **type filtering is essential** or the view is noise; album matching is
the same fuzzy problem as §6 (prefer MBID, else name+year). MusicBrainz is the default
catalogue; Discogs is the upgrade for edition/pressing detail and the haves/wants
signal. This is a natural fit for **M7** (stats/recommendations) and reuses the
provider-capabilities idea — a "discography/catalogue" capability a source can offer.

References: Lidarr (servarr wiki), beets `missing` plugin docs, Roon KB "albums in my
library vs outside", Discogs API (artist releases / masters / collection / wantlist).

---

## 8. Artwork storage & caching (acquire → cache → serve)

**Problem:** cover art comes from uneven sources — a `cover.jpg` next to local files,
an image embedded in tags, a `Folder.jpg` on an SMB share, the Cover Art Archive, or
(later) a streaming service's URL. Serving it well means: don't re-fetch the original
on every request (we currently read SMB covers over the wire per request, ~0.3 s each —
murder on an 11k-album grid), don't bloat the DB, and serve a small thumbnail to a grid
rather than a 2 MB original. Where should bytes live, and who fetches them?

**How others solve it** — Navidrome and Jellyfin converge on the *same* architecture:

- **Bytes live on disk, never in the DB.** Navidrome caches processed images under
  `{CacheFolder}/cache/images/` (hash-prefixed dirs); Jellyfin writes resized variants
  to `{ImageCachePath}/resized-images/{prefix}/{md5}.{ext}`
  (`src/Jellyfin.Drawing/ImageProcessor.cs`). The **DB stores only references +
  metadata** — Jellyfin's `BaseItemImageInfo` holds `Path`, `DateModified`,
  `Width/Height`, `Blurhash` (no pixels); Navidrome stores `image_paths` / uploaded-file
  names / cached external URLs. **Nobody stores originals as DB blobs.**
- **The resized cache is keyed by a hash of (source identity + all transform params +
  source mtime + a global version constant).** Jellyfin: `MD5(path + width + height +
  quality + format + dateModified + …)`, with a `Version` constant (`'3'`) that
  invalidates *every* variant when bumped (`ImageProcessor.cs`). Navidrome keys on
  `ArtworkID{Kind,ID,LastUpdate}` plus the size suffix. So a `?size=300` request maps to
  its own cached file; mtime change or version bump = automatic miss.
- **Generation is lazy (on first request).** Jellyfin is purely lazy — `ProcessImage()`
  checks `File.Exists(cacheFilePath)`, encodes via **SkiaSharp** only on a miss
  (`ImageProcessor.cs:197`). Navidrome is lazy too, but adds an **eager `CacheWarmer`**
  that pre-caches original + UI size at scan time (deferred until *after* the DB
  transaction commits, so the row exists) — best of both: instant first browse, lazy
  fallback. Image libs: Navidrome `golang.org/x/image/draw` + WebP; Jellyfin SkiaSharp.
- **Acquisition is source-aware with an explicit precedence.** Navidrome's
  `CoverArtPriority` default is `cover.*, folder.*, front.*, embedded, external`
  (`core/artwork/sources.go` chains readers); discs/artists have their own priority
  lists. Jellyfin's `LocalImageProvider` (order 0) looks for `poster/folder/cover.*` next
  to media, then remote providers, then embedded — a chain of `IImageProvider`s.
- **Eviction differs.** Navidrome runs an LRU "haunter" bounded by `ImageCacheSize`
  (default 100 MB); Jellyfin has **no automatic eviction** (manual version-bump /
  orphaning only). Covers are small, so a generous cap or keep-all is fine early.

**What Musicata adopts:** the shared model, mapped onto our provider abstraction.
*Provenance in the DB, bytes in a local cache, acquisition through the provider.*

- **DB** keeps artwork *provenance*, not pixels: source kind + original ref
  (local path / SMB share-relative path / URL) + a content hash + mtime. (Today
  `Album.artwork_path` already holds the original ref; this generalizes it.)
- **A managed cache dir** next to `musicata.db` (`.musicata/artwork/`), keyed by
  **content hash** (dedupes identical covers across a compilation / various-artists set)
  with **sized variants** (`{hash}@{size}.{ext}`) generated lazily; the `image` crate
  (MIT/Apache — AGPL-compatible) for decode/resize/encode.
- **Acquisition is a provider concern** (fits `ProviderCapabilities` / `ProviderHandle`):
  a provider returns artwork bytes (or a URL) for an item — local reads the file, SMB
  fetches it over the wire **once** and the cache holds it (also makes covers survive
  the NAS going offline), embedded extracts from tags, Cover Art Archive downloads (the
  candidate/review flow already exists), streaming services may just hand back a URL.
- **Serving** reads the cache (fast, offline-resilient); a `?size=` param selects a
  thumbnail — the real win for large-library grid/scroll performance. The SMB
  read-through I added is the *cache-populate* path, not the hot path.
- **Lazy first (on-request), then optional eager prefetch** (Navidrome's warmer) once
  scans can afford it. Start with one grid size + originals; expand sizes if needed.
  Eviction: keep-all with an optional cap initially.

This is the natural sequel to §4 (network never on the hot path) and §2 (cheap steady
state — reuse by mtime/hash). See `docs/roadmap.md` M3 for the staged plan.

**Implemented — the acquisition lane (embedded + external providers).** Bytes in the
cache, provenance in the DB, acquisition pluggable, exactly as above:
- **Embedded artwork** fills coverless albums from a track's tags: the scanner points
  `Album.artwork_path` at the audio file (`musicata-core`: `build_track`/
  `aggregate_track`/`extract_embedded_cover`), and the serve handler extracts + caches
  the picture on demand (local or SMB).
- **External providers** — a pluggable lane (`crates/musicata-server/src/
  artwork_providers.rs`, mirroring the music-source `ProviderHandle`/registry): an
  `ArtworkProvider` trait + a priority registry that tries **MusicBrainz-id matches
  first** (Cover Art Archive, then **fanart.tv** when an API key is set) then **text
  search** (iTunes/Apple, Deezer), skipping id-only providers when an album has no
  MBIDs. Each is a small sync `ureq` client (like `musicbrainz.rs`) with a shared
  per-provider rate limiter; parsing is pure functions (unit-tested with canned JSON).
- **Automatic fill** — `artwork_fill_pass` runs after each scan (toggled in the
  `/admin` **Settings** panel — a DB-backed setting, not a flag — default on): re-apply
  already-acquired covers (a rescan
  rewrites the albums table, wiping `artwork_url`), then fetch for still-coverless
  albums (capped per pass, downloaded → `ArtworkCache` → an `acquired_album_artwork`
  row, migration v19), reported on the scan's activity feed. A **negative `not_found`
  marker** stops the 30 s rescan from re-querying coverless albums (weekly retry). The
  serve handler checks the acquired row first.
- **ToS note:** Cover Art Archive is open; **iTunes** artwork is licensed "to promote
  store content" (a gray area widely used by Navidrome/beets/Jellyfin — low risk for a
  self-hosted personal server, attribute the source); **Deezer** asks for attribution;
  **fanart.tv** needs a free personal key. Text-search results are auto-applied but
  user-replaceable, and id-exact providers run first to avoid mismatches.
- **`?size=` thumbnails** (done) — the serve handler (`serve_sized_artwork`, `main.rs`)
  takes `?size=`, snaps it to a small `{128,300,600}` ladder (bounding variant count),
  resizes the original with the `image` crate in `spawn_blocking`, and caches the variant
  next to the original as `{key}.{size}.jpg` (the existing sharded `ArtworkCache`, keyed by
  the size suffix in the extension — the Navidrome/Jellyfin "size-suffixed cache key"
  pattern). Size-aware ETag; a decode failure falls back to the original (never a 500). The
  web album grid requests `?size=300`. Originals served unchanged when no size is asked.
- **Still open** (roadmap M3 §8): content-hash keying/dedup + invalidation, eager prefetch
  + a bounded cache.

**Where else artwork lives (candidate sources for the lane).** Adding one is a single
`ArtworkProvider` + a registry entry; the question is coverage vs. auth/ToS cost:
- **TheAudioDB** — free key (`123`, ~30 req/min; private key via Patreon), text+MBID,
  good album *and artist* art. **Easiest high-value next add.**
- **Wikidata → Wikimedia Commons** — P18 image via the release-group's Wikidata link;
  free and **openly licensed (CC)** — the best license story — but sparse for albums and
  multi-step (MB → Wikidata → Commons `imageinfo`). Good long-tail filler.
- **Discogs** — huge coverage (obscure/vinyl), but **images require OAuth/token auth**
  (key+secret) and are rate-limited; ToS restricts use. Medium effort.
- **Spotify Web API** — top-quality covers (and artist images), but needs **OAuth client
  credentials** (app registration) and display ToS; naturally rides the streaming-
  provider work (prior-art §9).
- **Last.fm** — ❌ album-art endpoints were **removed/deprecated** (~2019, returns a
  placeholder); skip for covers (still useful for scrobbling/metadata).
- **Better matching, not a source — implemented:** **AcoustID/Chromaprint** audio
  fingerprinting turns an *untagged* file into an MBID, unlocking the id-exact providers
  (CAA, fanart.tv) — the biggest quality lever for poorly-tagged libraries. Pure Rust:
  `symphonia` decodes ~120 s of audio, `rusty-chromaprint` (MIT) computes the
  fingerprint, and a `ureq` client queries AcoustID (`fingerprint.rs`). A
  `fingerprint_pass` runs after each scan **before** the artwork pass, finds untagged
  tracks, decodes+fingerprints+looks-up on `spawn_blocking` (rate-limited to AcoustID's
  3 req/s, capped per pass, negative-cached), and writes a `track_fingerprint` row
  (migration v21, FK-less); `album_musicbrainz_ids` `COALESCE`s observation ids with
  fingerprint ids, so the artwork lane then reaches CAA/fanart.tv. Gated by a Settings
  toggle; needs Musicata's own free AcoustID **application** key compiled in (no-ops
  until set). Auto-*applying* the MBIDs' MusicBrainz metadata (retagging) is a separate
  follow-up.

**A second axis — artist artwork (not yet implemented).** Today the lane fills *album*
covers only. Artist images/backgrounds (for artist pages + a now-playing backdrop) are a
natural extension reusing the same lane: **fanart.tv** (client already exists — artist
backgrounds/banners/logos), **TheAudioDB** (artist thumb/fanart), **Deezer**/**Spotify**
(artist picture). Recommended order: TheAudioDB (album+artist, free) → artist-artwork
axis (fanart.tv/TheAudioDB/Deezer) → Wikidata/Commons (license) → Discogs/Spotify (auth).

References: Navidrome `core/artwork/` (sources/cache_warmer/reader_resized) +
`utils/cache/file_caches.go`; Jellyfin `src/Jellyfin.Drawing/ImageProcessor.cs`,
`BaseItemImageInfo`, `LocalImageProvider`, `ItemImageProvider`. Provider APIs: iTunes
Search, Deezer API, Cover Art Archive, fanart.tv.

---

## 9. Streaming-service & external-source integration

**Problem:** which non-local sources can Musicata add as providers, can we get
*playable audio* (not just metadata), can it be done **self-contained in Rust**, and
what's the legal/ToS bottom line for an **AGPL project that publishes its source**? (See
the M9 roadmap task. Researched Jun 2026 via two fact-checked passes; per-service
findings below are adversarially verified, sources inline.)

**Architecture, confirmed by Music Assistant** (`developers.music-assistant.io`): the
split we already have is the right one. MA separates **Music Providers** (sources) from
**Player Providers** (targets), advertises per-provider `supported_features`
(capability negotiation — our `ProviderCapabilities`), and plays via a **two-phase**
`get_stream_details()` → `get_audio_stream()`. It uses exactly three audio-delivery
shapes, a useful taxonomy for our `resolve()`: **(a) external executable** (Spotify via
librespot), **(b) direct URL** (YouTube Music), **(c) expiring HTTPS URL** (Qobuz).
Metadata/scrobbling services (Last.fm/ListenBrainz/MusicBrainz/Discogs) are a *separate
metadata/plugin lane*, never a music source.

### Tier A — open / free / self-hostable: highest value-per-risk, do these first

DRM-free, full-quality audio over public/stable APIs, ~zero ToS risk, plain-HTTP Rust
clients. The natural expansion for a local-first server:

- **Upstream OpenSubsonic servers (Navidrome/Gonic) — the standout.** MA ships a
  Subsonic source streaming **lossless FLAC up to 24/192, no DRM**
  (`music-assistant.io/music-providers/subsonic/`). **Funkwhale also speaks Subsonic**
  (`docs.funkwhale.audio/developer/api/subsonic.html`, ~27 endpoints), so **one
  OpenSubsonic client ingests Navidrome + Gonic + federated Funkwhale pods**. We already
  implement the OpenSubsonic *server* surface — this is writing the *client* of a
  protocol we know; an `opensubsonic` Rust crate exists. Caveat: requires
  `getOpenSubsonicExtensions` (works on Gonic/Navidrome/Funkwhale, not legacy
  Subsonic/Airsonic).
- **Jellyfin / Plex / Emby as upstream** (MA ships all three; their Plex is unmaintained,
  Jellyfin best-effort).
- **Podcasts** (Podcast Index / RSS), **Internet Archive** audio, **Jamendo** (CC; public
  REST API with a free `client_id`, FLAC/OGG/MP3; `jamendo-rs` crate) — all DRM-free and
  shipped in MA/Mopidy. RadioBrowser (already integrated) is the same class.

### Tier B — the commercial big three: feasibility **Qobuz > Tidal > Spotify**

All three require an **unofficial, reverse-engineered client violating the service's
ToS**; official APIs give metadata but **forbid serving raw audio to non-certified
players** (Spotify's `streaming` scope and Web Playback SDK are Premium- and
browser/EME-only; TIDAL's SDK Player module is the only sanctioned playback). So audio
always means RE.

- **Qobuz — most feasible, and the only big-three lossless that needs no CDM.** Auth is
  **MD5 request-signing, not DRM** — signed **FLAC** URLs are obtainable in Rust without
  Widevine. Mature Rust prior art: **`qobuz-api-rust`**, **`hifi-rs`**, and
  **MoosicBox** (a Rust self-hosted server whose Qobuz package fetches app credentials
  via *Spoofbuz* then logs in with username/password). Needs a Qobuz subscription;
  fragility is the Spoofbuz credential scrape. *(github.com/loxoron218/qobuz-api-rust,
  iamdb/hifi-rs, MoosicBox/MoosicBox)*
- **Tidal — feasible for lower tiers, DRM-gated for HiFi.** **`tidalrs`** is an unofficial
  Rust client that returns audio stream URLs over **DASH-MPEG across four quality tiers**;
  MoosicBox's Tidal package also gets real stream URLs. But **HiFi (High/Max) is
  DRM-protected** → lossless is Widevine-gated, lower tiers obtainable.
  *(github.com/phayes/tidalrs, Mastermindzh/tidal-hifi)*
- **Spotify — most mature Rust tooling, least feasible for the goal.** **librespot** is a
  mature, in-process **Rust** Spotify client (so "self-contained Rust" is genuinely
  possible here) — but it is **lossy Ogg Vorbis only** (lossless is *not* delivered over
  the same transport librespot handles — a "FLAC reuses the same key path" claim was
  **refuted 0-3**, librespot issue #1583), **requires Premium**, and the project warns
  connecting **may risk account bans**. Spotify also has the most active anti-OSS
  enforcement (copyright C&D against ReVanced; a FOSDEM 2026 RE talk). *(librespot-org/
  librespot, developer.spotify.com/terms)*

### Tier C — other commercial: work, but fragile / lossy / scraping

- **Deezer** — the only *other* lossless (16/44.1 FLAC), but via **Blowfish-encrypted ARL
  streams**; Rust `deezer`/`deezer-rs` are **metadata-only**, decryption is bespoke (the
  `pleezer` crate does it). Breaks periodically. Conditional at best.
- **YouTube Music** — huge free catalog (free accounts work), **lossy AAC 256k**;
  **`rustypipe`** (Rust Innertube client) but since Aug 2024 needs the separate
  **`rustypipe-botguard`** PO-token helper or streams 403 — operationally fragile.
- **SoundCloud** (`rsoundcloud`, scraped `client_id`, lossy) and **Bandcamp** (no API,
  HTML scraping, ~MP3 128k) — modest value, bespoke Rust.

### Infeasible for self-contained playback

- **Apple Music** — catalog API exists, but playback is **locked to Apple's MusicKit
  player**; no raw audio to third parties.
- **Amazon Music** — Widevine DRM **and** a ToS that *explicitly forbids* platforms where
  the user can install software or access the filesystem — it bans a self-hosted server
  by design (`developer.amazon.com/docs/music/`).

### Legal bottom line

Every commercial path is **unofficial and ToS-violating**, and decrypting/working around
DRM (Spotify, Tidal HiFi, Deezer, Amazon) raises **DMCA §1201 anti-circumvention**
exposure. Precedent cuts both ways: RIAA's youtube-dl takedown was reversed (EFF, 2020),
but Spotify successfully C&D'd ReVanced (2025). An AGPL project that **publishes the
source** is more exposed than a private tool. Non-DRM RE (Qobuz MD5 signing, official-API
metadata, scraping) is "merely" ToS-breaking; DRM circumvention is the bright legal line.

**What Musicata adopts:** build **Tier A first** (real APIs, DRM-free, Rust-easy,
no legal exposure) — start with the **OpenSubsonic/Funkwhale upstream client** (huge
leverage, reuses our Subsonic knowledge), then podcasts/Internet Archive/Jamendo. Treat
the commercial big three as **opt-in, cargo-feature-gated, user-supplies-own-credentials
providers** (the SMB precedent), and if/when we do one, **Qobuz is the first target**
(lossless, no CDM, working Rust prior art). Defer/skip Spotify (lossy + ban risk +
enforcement), Apple/Amazon (architecturally impossible). The capability model already
fits: a streaming provider advertises `can_search`/`can_browse`/`can_stream`, no
`can_scan`, and `resolve()` returns a `StreamSpec` (direct or expiring URL) — exactly the
radio-provider shape generalized. Keep DRM-circumvention code out of the default build.

References: Music Assistant docs + provider list; MoosicBox (Rust Tidal/Qobuz server);
librespot (+ issue #1583); tidalrs; qobuz-api-rust / hifi-rs; deezer-rs / pleezer;
rustypipe (+ botguard); Funkwhale Subsonic API; Jamendo API; EFF youtube-dl/RIAA;
Spotify↔ReVanced (TorrentFreak). Companion: the standalone Spotify/Tidal/Qobuz and
broader-landscape research reports.

---

## 10. Progressive media streaming — low-latency playback from a network source

**Problem:** when the audio bytes live on a network source (SMB), how do we get a
browser `<audio>` playing *fast*? A browser opens a track with `Range: bytes=0-`
(open-ended). If the server resolves that to "the whole file" and **buffers it before
sending byte one**, playback waits for the entire file to cross the wire — measured
**~400–600 ms** to first byte for a 6.5 MB FLAC over SMB (and TTFB ≈ total, i.e. no
streaming at all). Local files don't show this because the OS already streams them.

**How Jellyfin does it (chunked, never whole-buffer):**
- Direct play returns ASP.NET's `PhysicalFileResult { EnableRangeProcessing = true }`
  (`Jellyfin.Api/Helpers/FileStreamResponseHelpers.cs:105`), so range parsing, `206`,
  and **incremental chunked writes** are handled by the framework — the file is never
  read whole into memory.
- Remote HTTP sources: forward the client's `Range` upstream and pull the body with
  `HttpCompletionOption.ResponseHeadersRead` (returns after headers, streams the body
  through — no buffering), `FileStreamResponseHelpers.cs:45-57,96`.
- Copies use an **80 KiB** buffer from `ArrayPool` — `IODefaults.CopyToBufferSize =
  81920` (`MediaBrowser.Model/IO/IODefaults.cs:13`); `StreamHelper.CopyToAsync`
  (`src/Jellyfin.LiveTv/IO/StreamHelper.cs:14`) fires an `onStarted` callback on the
  **first** chunk, so playback begins as soon as the first bytes arrive, not the range.
- Backpressure/throttling is at the **source** (pause ffmpeg when the client is far
  ahead — `TranscodingThrottler`), not an output-side rate clamp.

**What Musicata does:** stream the SMB byte range in chunks instead of buffering it.
`SmbProvider::read_range_stream` (`crates/musicata-server/src/smb.rs`) opens the file
once and a spawned reader feeds a **small bounded `mpsc` channel** (depth 4); the HTTP
handler wraps the `ReceiverStream` in `Body::from_stream` (`smb_stream_response` in
`main.rs`). The first bytes reach the client after a single read; the bounded channel
is natural backpressure — if the browser pauses/seeks/closes, the channel fills, the
reader parks, and **no more SMB reads happen** (no whole-file buffering). `Content-
Length` is still exact (the range length), and open-ended `bytes=0-` is served as a
streamed full-content `206` (full seek support, no follow-up range request).

**The non-obvious tuning — bigger chunks win on SMB (opposite of local).** Jellyfin's
80 KiB is right for local files (no per-read latency). Over SMB **every read costs a
round-trip**, so too-small chunks throttle the fill rate and *delay* `canplay` even
though they lower time-to-first-byte. Measured click→audible for a FLAC over SMB by
chunk size: **128 KiB→515 ms · 256 KiB→335 ms · 512 KiB→226 ms · 1 MiB→176 ms**. We
chose **1 MiB** (`STREAM_CHUNK_BYTES`) — the knee of the curve, ~3.5× faster than the
old whole-file buffer, bounded memory (channel depth × chunk). So: "smaller chunks
lower latency" is true for *first byte* and memory, but for a round-trip-bound source
the metric that matters (time to *enough buffered to play*) favors larger chunks.

**Where the lag is NOT:** measuring the click→audible path end-to-end (headless CDP +
API timing) showed the **UI/comms overhead is ~2–8 ms** — the browser player sets
`<audio>.src` and calls `.play()` synchronously inside the click gesture, and the
`play_tracks` command POST runs *in parallel* (it doesn't gate audio start); the
`driveBrowserAudio` WebSocket echo reuses the same URL so there's no second fetch
(`srcReset=0`). The lag was essentially all stream-fetch latency, not the
track→player handoff. Local-track click→audible is ~50–100 ms for comparison.

**Related (same "network must not stall the server" theme):** the SQLite pool runs in
**WAL** (`crates/musicata-storage/src/lib.rs`, `journal_mode(Wal)` + `synchronous
NORMAL`). Under the old rollback (`delete`) journal, the scan's full-library
`save_library` rewrite held an exclusive write lock and every concurrent read (e.g.
`/api/library/summary`) hit the 5 s busy-timeout and 500'd; WAL lets reads run
alongside the writer.

---

## 11. Artist identity & variant-name merging

**Problem:** the same artist appears under different name strings — "Fela Kuti" vs "Fela
Anikulapo Kuti", "The Beatles" vs "Beatles", "Beyoncé" vs "Beyonce", "Marina & the
Diamonds" vs "Marina and the Diamonds" — and fragments into separate artist entries.
Musicata today keys identity as `stable_id("artist", name.to_ascii_lowercase())`
(`musicata-core/src/lib.rs`), so only **case** collapses; diacritics, leading "The", and
genuine variants all split.

**How the others do it (researched — nobody fuzzy-merges):**

- **Jellyfin** — identity is the **normalized name** (`GetCleanValue`: diacritics removed,
  lowercased, punctuation→space, whitespace collapsed) with a **MusicBrainz Artist Id**
  user-data key when present (`MusicArtist.GetUserDataKeys`: `Artist-Musicbrainz-{mbid}`
  else `Artist-{name-no-diacritics}`). Its **only** automatic merge is **case-insensitive**
  (a 2026 migration `MergeDuplicateMusicArtists` groups `Name.ToLowerInvariant()`). "Fela
  Kuti" vs "Fela Anikulapo Kuti" stay **separate**. Multi/featured artists are split on
  `/ ; | \` + "feat."/"featuring", with a hardcoded whitelist (AC/DC, Smith/Kotzen).
- **Navidrome** — artist **PID is name-only and not configurable** (album/track PIDs are
  MBID-first, artists are not), so it can't even *split* two same-name artists that have
  distinct MBIDs (issue **#3964**, filed by Picard's lead dev). Its creator calls
  MBID-based identity "the ideal path forward" but blocked on **patchy per-role MBID
  coverage** (composer/engineer tags often have no MBID slot). No manual-merge UI; the
  documented fix is *consistent tagging*. Splits multi-value `ARTISTS`/`ALBUMARTISTS` tags,
  else a separator list (`/`, ` / `, ` feat. `, `; `, …).
- **MusicBrainz/Picard** — the canonical model: a stable **Artist MBID** plus **aliases**
  (artist-name / legal-name / search-hint, locale-tagged) that map every variant spelling
  onto the one MBID ("Fela Anikulapo Kuti" *is* an alias of the Fela Kuti MBID). Picard
  writes `MUSICBRAINZ_ARTISTID` (multi-valued), `ARTISTS`, `ARTISTSORT`; its "Use
  standardized artist names" rewrites credited variants to the canonical name at tag time.
- **Roon** — canonicalizes against a **curated cloud DB** (TiVo/Rovi + MusicBrainz):
  "there is only one Beethoven." Powerful, but it's *match-to-curated-entity*, not local
  string fuzzing; unidentified local files degrade to tag munging.
- **beets** — identity is `mb_artistid` from MusicBrainz; lacking an MBID it keeps things
  distinct rather than guessing. `duplicates` plugin dedups tracks/albums by MBID keys.

**Why nobody substring/fuzzy-merges:** it silently collapses genuinely distinct same-name
artists — multiple "John Williams" (film composer vs classical guitarist), the metal-scene
"Death"/"Depression" bands — and substring is worse ("Marley" ⊂ "Bob Marley"/"Ziggy
Marley"). It's lossy and hard to undo.

**What Musicata should adopt (for a library with ~0 MBIDs):**

1. **Safe normalization only, automatically** — extend the identity key beyond case to
   strip diacritics, fold a leading "The ", and normalize punctuation/whitespace (Jellyfin's
   `GetCleanValue`). This merges the *unambiguous* cases (Beyoncé/Beyonce, The
   Beatles/Beatles) and nothing risky. Keep the original string as the **display name**.
   (Re-deriving the key re-groups artists — fine, artist ids are derived, not canonical
   track ids.)
2. **MBID-first identity with a name fallback** (kgarner7's `name|mbid` model from #3964):
   `mbid` when enrichment has one, else the normalized-name hash — so `A|1234`, `A|2345`,
   `A|` are all distinct and adding MBIDs later auto-merges/splits correctly. Don't drop
   artists lacking an MBID. This is what makes "Fela Kuti"≡"Fela Anikulapo Kuti" resolve
   *for free* once MusicBrainz enrichment runs (they share one artist MBID with the other
   as an alias).
3. **A manual merge / alias tool** — the honest answer for true variants with no MBID, and
   the thing every other server *lacks*. A user-curated "treat these names as one artist"
   mapping, reversible, lives **in the product** (DB + `/admin`), mirroring MusicBrainz
   aliases. Never fuzzy-merge automatically.

---

## Conventions these led to

- **Enum dispatch over `dyn`** for provider/player handles (async methods, object
  safety). New backend = one variant + arms.
- **Capabilities are advertised, not assumed** — callers skip work a source can't do.
- **Source layer ≠ transport layer** (Roon) — scanning/library vs playback/output are
  separate trait families.
- **Cheap steady state** — compare mtime/size, reuse parsed metadata, cache directory
  listings, watch instead of poll, push instead of poll.
- **Network is never on the hot path of a request** — connect/scan in the background
  with timeouts; the web port binds before any scan.
- **Stream media bytes, never whole-buffer** — pipe a network source in bounded chunks
  with backpressure so playback starts on the first read; size the chunk to the
  source (round-trip-bound SMB wants ~1 MiB, not 80 KiB). See §10.
