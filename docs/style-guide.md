# Web UI style guide

Conventions for the Musicata web app (`crates/musicata-server/static/`). It's a
no-build, vanilla HTML/CSS/JS PWA — keep it that way.

## Surfaces

- **Player** (`/`, `index.html` + `app.js`) — listening: library, playlists, radio,
  the now-playing transport, and the active-output switcher. Playback only.
- **Admin** (`/admin`, `admin.html` + `admin.js`) — management: music sources,
  players & zones, and the activity/error log. Anything administrative or
  long-running lives here, never crowding the player.

Keep the split clean: if a control manages configuration or shows background
progress/errors, it belongs on `/admin`.

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
- Confirm destructive actions (`window.confirm`). Disable a submit button while its
  request is in flight.

## PWA / caching

Static assets are embedded at compile time (`include_str!`) and cached by the service
worker (network-first). **Bump `CACHE` in `sw.js`** whenever a static asset changes,
so clients pick up the new version on reload.
