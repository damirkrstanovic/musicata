# Decision log

Short, dated records of non-obvious choices made while building Musicata — the "why" behind
work that isn't self-evident from the code. Newest first. Referenced from `roadmap.md`.

## 2026-06-27 — Roadmap sweep: M7, M9, M10, M12

A batch of milestone work was done autonomously; the judgement calls are recorded here.

### M7 — Listening stats (session/streak + favorites stats)

- **Scope:** shipped a read-only **stats API** (`GET /api/history/stats`) and the storage query
  behind it, covering the two open M7 stats items — *session/streak views* and *favorites
  stats* — plus library-wide play/skip totals. Left as a follow-up: a web view for it and the
  remaining "richer playback events" (loved/disliked/rated as discrete events; favorites already
  cover "loved"). Rationale: the data + API is the load-bearing, testable part; the UI is a thin
  read-only follow-up that would also need the smoke suite extended, so it's deferred rather than
  rushed.
- **A "session" is a run of plays with < 30 min between consecutive listens.** No `session_id`
  column was added — sessions are derived in Rust from ordered played-listen timestamps, so the
  definition can change without a migration and historical data participates immediately.
- **A "streak" is consecutive UTC calendar days with ≥ 1 played listen.** UTC (not local) is
  deliberate: the server has no reliable user timezone, and UTC keeps the query and the tests
  deterministic. Current streak counts back from today (or yesterday, so a streak isn't "broken"
  until a full day is missed).

### M9 — Podcasts provider + plugin isolation

- **Built a Podcasts (RSS) source, not Jamendo, as the next provider.** Both were open M9 items;
  podcasts won because RSS needs no API key or account (Jamendo needs a `client_id`), maps
  cleanly onto the existing **STREAM_ONLY browse/resolve** provider shape (episodes are streams
  with known enclosure URLs, exactly like radio stations), and is fully unit-testable offline
  from a fixture feed. It is **feature-gated `provider-podcast`** (default-on, like
  `provider-opensubsonic`) and added through the same `/api/sources` path. New dep: `quick-xml`
  (MIT) for RSS parsing.
- **A podcast source is browse-only (`STREAM_ONLY`), not scanned into the library.** Episodes are
  fetched and parsed on demand in `browse()` (the feed URL lives in the source's `host` column,
  reusing the OpenSubsonic field convention) — never on the scan hot path. This mirrors the
  internet-radio provider and keeps podcast feeds (which change often and can be huge) out of the
  canonical track tables.
- **No `/admin` UI for adding a podcast yet** — it's reachable via `POST /api/sources` with
  `{"kind":"podcast","host":"<feed-url>"}`. UI is a follow-up; the provider and its tests are the
  substance.
- **Plugin isolation (the standing "evaluate isolation" item) is now decided** — see
  `plugins.md`. Short version: first-party providers stay **in-process enum-dispatch**;
  untrusted third-party plugins, if ever, target the **WASM component model** with an
  out-of-process subprocess as the fallback. No third-party plugin loading ships now.

### M10 — Player providers & endpoint capabilities

- **Shipped `PlayerCapabilities`** (advertised per player, like `ProviderCapabilities` for
  sources) and surfaced it on the player descriptor / `GET /api/players`. This completes the
  "define PlayerProvider and endpoint capabilities" item: a controller can now ask what a player
  supports (seek, volume, queue editing, streams) instead of probing.
- **Per-player endpoint auth token: designed and documented, enforcement deferred.** See
  `player-auth.md`. The token column + issuance are *not* added yet because the only thing that
  would present it — a native non-browser endpoint that registers *itself* — does not exist; the
  browser, MPD and Snapcast players are all **server-initiated** and already covered by the
  user-session `require_auth` middleware. Adding an unenforced token now would be security
  theatre. The design is committed so it lands with the native endpoint (the M10 "native endpoint
  prototype" task).

### M12 — Packaging, security & operations

- **Release builds, the systemd unit, and the Docker image are done** (commit `557b2f3`:
  `.github/workflows/release.yml`, `packaging/musicata.service`, `Dockerfile`) and the roadmap is
  updated to reflect that. `docs/deployment.md` documents running them.
- **Backup/restore is documented** (`deployment.md`) on top of the existing library
  export/import; the database + artwork cache live under one state dir (`/var/lib/musicata`), so
  backup is "copy that directory while stopped, or snapshot it."
- **Source secrets (SMB/MPD/OpenSubsonic passwords) stay plaintext at rest — deliberately, for
  now.** Encrypting them only helps if the key lives somewhere the DB-reader can't reach; without
  an OS keyring or hardware-backed key the key would sit next to the database and the encryption
  would be decorative. Given Musicata's **LAN-first** posture, the honest mitigation is
  filesystem permissions (the systemd unit already runs as a locked-down system user with state
  in `0750` `/var/lib/musicata`) plus clear documentation. Real encryption is revisited if/when
  an OS-keyring integration lands. Documented in `deployment.md` and the roadmap.
