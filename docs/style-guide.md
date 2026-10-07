# Web UI style guide

Conventions for the Musicata web app (`crates/musicata-server/web/src/`). It's a
**Svelte 5 + TypeScript + Vite** app, built by `build.rs` and embedded into the server
binary via `rust-embed` from `web/dist/`. Components are `PascalCase.svelte`.

## Surfaces

- **Browse** — artists, albums, tracks, favorites, search and library filters.
- **Listen** — queue, playlists, similarity mixes and internet radio stations.
- **Settings** — administrator-only configuration inside the shared player shell:
  Music sources, Outputs & groups, Sound profiles, Metadata & artwork,
  History & services, and System & accounts. Categories are tabs; search indexes
  named destinations and keywords, never current values or credentials.

The `/` and compatible `/admin` entry points mount the same player component tree.
Internal Settings navigation must preserve the audio element, renderer, output,
WebSocket and transport. `/admin` starts at Settings and remains administrator-gated.
Keep the activity split clean: configuration and maintenance progress belong in Settings.
Playback work initiated by the listener (creating a mix or finding the next song) shows
its loading, empty, and error states in the player, where the action was taken.

## Browsing (player)

Only Browse carries the **segmented switcher** — Tracks · Albums · Artists — driven by
the runes nav store (`lib/nav.svelte.ts`, a `Nav` instance); the sidebar nav keeps the
*smart* views (Favorites / Recently / Most) plus Playlists and Radio. Browse state (filters,
sort, paging) lives in `lib/browse.svelte.ts`. **Albums and Artists render as cover-forward
cards** in a full-width grid (`.browse-grid` → `.card`/`.album-card`/`.artist-card`), paged
by infinite scroll and a contextual **sort** select. Browsing is **master→detail**: a card
opens a **hero header** (`.detail-hero` — large cover, serif title, a clickable artist link,
year · tracks · duration, and **Play** / **Shuffle**) above the tracklist (album) or an
albums grid (artist). Drill-downs create browser history entries through the nav store;
the visible **Back** control and browser Back/Forward use the same history entries,
including the navigation drawer, Now Playing, and queue. Entries store the route and
scroll position; paged grids load enough content to restore a deep browsing position. Album covers in grids request
the `?size=300` thumbnail; hero covers `?size=600`.

## Listening flow across screens

A fresh visit on any screen starts with album covers. The last Albums, Artists or Tracks
choice is remembered in this browser; Browse returns to that choice on desktop. Detail routes
and temporary playback panels are restored by history, not saved as the browsing preference. Persistent bottom destinations are
**Library · Playlists · Now Playing**, with the compact player above them. Artists and
Tracks remain available in the browsing switcher. The top bar keeps the current output
visible and exposes Back whenever there is an in-app history entry.

The compact player's title and artwork open Now Playing: controls followed directly by
the active queue, with its current track highlighted. Saved playlists have a separate
browse screen. Back closes the current playback/drawer view before returning through
library navigation, and Forward restores that view. Desktop retains its three-column
layout and exposes Browse, Listen and Settings in the sidebar, with listening shortcuts.
Clicking its current song or Now Playing opens the queue alongside the transport, leaving
the library available. Output selection and playback controls stay visible. The optional
install prompt sits in the page flow on all screen sizes so it cannot cover browsing controls.

Sound-profile creation, import, deletion, room-filter uploads and browser device bindings
live in Settings → Sound profiles. Profile selection, bypass and volume leveling remain
in the playback EQ drawer. Saving a new profile in Settings does not activate it; selecting
an existing profile for management must not change the output's listening selection.
Equipment correction and listening adjustments have separate selection and bypass controls,
remembered per output. Neutral removes the listening adjustment without changing correction.
The response curve describes their combined processing. A master bypass retains both choices.
Profile-management mutations require administrator authorization; listeners can read profiles
and change per-output selections. Built-in presets are supplied by one shared preset file,
so selecting one never requires a profile-management write.
Settings imports classify profiles as headphone correction, speaker/room correction, or a
listening adjustment. Room impulse responses belong only to equipment correction.

Tracks offer Play, Add to queue, Play next and Add to playlist. Queue and similarity mixes
can save their ordered library tracks as playlists; transient streams cannot become library
track entries. Playlist details support naming, ordering and removing individual occurrences
without changing the active playback queue.

## Aesthetic

Warm hi-fi, dark, gold accent. Use the CSS variables in `:root` (`--bg`, `--panel`,
`--panel-strong`, `--text`, `--muted`, `--line`, `--accent`, `--ok`, `--danger`, …) —
never hard-code colors. Display serif (`--font-display`) for headings, UI sans
(`--font-ui`) for body, mono (`--font-mono`) for technical text (paths, errors).
Soft gold focus ring (`--accent-soft`), 1px `--line` borders, rounded corners.

## Forms & inputs

**Size every field to the data it holds — never stretch all inputs to one width.**
A host is wider than a port; a password or a name is not full-bleed. Oversized
fields read as sloppy and make the form harder to scan.

- Give each field a class and set `flex-basis` + `max-width` in `ch`/`rem` sized to
  typical content (see the `.field-host`/`.field-share`/`.field-port` rules in
  `styles.css`). Let fields wrap (`.field-grid { flex-wrap: wrap }`) rather than
  forcing a rigid row.
- Rough guide: port ~6ch, year ~6ch, share/username ~12–14ch, host/`host:port`
  ~16–18ch, display name ~14–16ch, path/URL ~22–26ch, free text — flexible.
- Label every field (`<label class="field"><span>…</span><input></label>`); mark
  optional ones `<em>(optional)</em>`. Placeholders show an example value, not the
  label.
- One primary action per form (`.primary-button`); secondary/destructive actions use
  `.ghost-button` (+ `.danger` for remove/delete).

## Feedback & errors

- Long work (scans, connects) runs in the **background** and is **never** blocked on
  in a request handler that holds a UI; report progress and outcome via the activity
  log (`/api/activity`) shown on `/admin`.
- Show the **root cause**, not a status code: surface the API's
  `{ error: { message } }` body (see `apiSend`/`apiJson`). Errors get the mono font
  and `--danger`.
- **Never use native browser dialogs** (`window.confirm`/`alert`/`prompt`) — they're
  unstyled and silently suppressed in installed PWAs/mobile. Confirm destructive actions
  with the in-product `confirmAction({ title, message, confirmLabel })` modal; collect
  input with `promptText(...)` or an inline form. These helpers (`confirmAction`/`promptText`/
  `openModal`) live in one shared module, `lib/modal.ts`, backed by `lib/Modal.svelte`.
  Disable a submit button while its request is in flight.

## PWA / caching

The Vite build emits content-hashed bundles (`*.[hash].js/.css`); the server embeds
`web/dist/` via `rust-embed` and serves the hashed assets `immutable` while the HTML entries
are `no-cache`. The service worker is generated by `vite-plugin-pwa` (`autoUpdate`), which
precaches the app shell and busts its cache automatically on every content-hash change —
there is **no hand-maintained `sw.js` / `CACHE` constant to bump**. A new build is picked up
on reload because the no-cache HTML references the new hashed bundles.
