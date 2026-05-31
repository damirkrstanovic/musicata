# Prior art — how other music servers solve our problems

A living knowledge base: for each hard problem Musicata hits, how the reference
projects (Roon, Jellyfin, Navidrome, beets, Picard, Music Assistant, Mopidy) solve
it, and what we adopted — with pointers to our code. Covers: provider/plugin
ecosystem, incremental scanning, OpenSubsonic, SMB access, background-work UX, and
metadata sourcing & conflict resolution. When you tackle one of these areas, read the
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
- **MusicBrainz Picard**: **Cluster** (group files by album/artist tags) → **Lookup**
  (query MB by existing tags/MBIDs) → **Scan** (AcoustID acoustic fingerprint when
  tags don't resolve). Embedded MBIDs load the exact release directly. Canonical-
  source-wins: saving rewrites tags from MB.

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
folder — consensus #3, #4); **MBID-first auto-resolution** + a **confidence gate** for
text matches before auto-applying MB data (consensus #1, #2); a **tag-mapping/
multi-value normalization** layer (Navidrome `mappings.yaml`); **AcoustID** as the
last-resort matcher (consensus #6). The designed ladder is in `docs/metadata.md`
(§Metadata Sources); this section is the *why* behind it.

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
