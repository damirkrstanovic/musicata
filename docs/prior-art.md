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
only new/changed files are opened. A `notify` recursive watcher on the local root
(`spawn_library_watcher` in `main.rs`, 2s debounce) gives instant local updates;
network shares have no watcher and rely on the periodic pass (`LIBRARY_RESCAN_INTERVAL`,
60s — we can't use Jellyfin's 12h because SMB has no watcher). `detect_library_changes`
only decides whether to *write*.

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
