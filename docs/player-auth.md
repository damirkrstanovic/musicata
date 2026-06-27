# Player endpoint authentication (M10)

Status: **shipped** (2026-06-27) — the per-player token mechanism is implemented and enforced,
**and the self-registering `native` endpoint kind that consumes it is complete and verified**
(`crates/musicata-endpoint`; see [native-endpoint.md](native-endpoint.md)). See `decisions.md`.

## The problem

Milestone 10 calls for authentication *between the server and players/endpoints* — distinct
from the user↔server auth that already ships (`crate::auth`, M12). Today any device on the LAN
can `POST /api/players` to register itself, and any authenticated user can drive any player.
For the **server-initiated** backends that exist now — the browser player, an MPD instance the
operator points the server at, and Snapcast — that's fine: the server reaches *out* to them, so
there's nothing for the endpoint to prove, and the user-session `require_auth` middleware
already guards every `/api/players/*` channel (commands, state, the WebSocket via cookie or
`?token=`).

The gap opens with a **self-registering native endpoint** — the "lightweight native endpoint
prototype" also under M10. When a device registers *itself* and then receives playback
commands, two questions appear that don't exist for server-initiated players:

1. **Endpoint → server:** when the device opens its command/state channel, how does the server
   know it's the same device that registered (not another LAN host hijacking the player id)?
2. **Server → endpoint:** how does the device know a command really came from this server?

## The design

A **per-player bearer token**, issued once at registration, mirroring the forward-compatible
posture Snapcast already uses (write the auth config now, enforce when the moving parts exist).

- **Issuance.** `POST /api/players` for a self-registering kind generates a 256-bit random
  token, stores **only its SHA-256** in a new `players.auth_token_hash` column (migration), and
  returns the cleartext **once** in the registration response. This matches how user sessions
  are stored (hash at rest) — a DB leak doesn't yield usable player tokens.
- **Presentation.** The endpoint presents the token on its own channels —
  `Authorization: Bearer <token>` for `POST /api/players/{id}/commands` and `GET
  /…/state`, and `?token=` for the WebSocket (the query-param path `require_auth` already
  understands).
- **Enforcement scope.** Only players that *have* a token hash are checked, and the check is
  **in addition to** user auth, not instead of it: a human controller still authenticates as a
  user; the token authenticates the *endpoint*. Server-initiated players (browser/MPD/Snapcast)
  carry no token and are unaffected — so nothing that works today breaks.
- **Server → endpoint** is the native endpoint protocol's job (the endpoint pins the server URL
  it registered with and can carry a server-issued nonce); it's specified alongside that
  prototype, not bolted onto the current HTTP handlers.

## What shipped

The mechanism above is implemented:

- **Storage:** `players.auth_token_hash` (migration v30, NULL for the server-initiated
  backends). `set_player_auth_token_hash` / `player_auth_token_hash`.
- **Issuance:** `POST /api/players` accepts `"issue_token": true`; the response then carries
  `auth_token` **once** (only its sha-256 is stored). `auth::issue_player_token()` mints it.
- **Enforcement:** `require_auth` accepts a valid player token (Bearer or `?token=`) for that
  player's own channels — `/api/players/{id}/{state,commands,ws}` — *in place of* a user session,
  **and** for the audio streams an endpoint must fetch to play (`GET /api/tracks/{id}/stream`,
  authorized by any valid endpoint token via `player_token_exists`). It is additive: tokens are
  only consulted when user auth fails. Management paths (PATCH/DELETE `/api/players/{id}`) and
  every other path stay user-gated.
- **Tested:** an HTTP test proves a tokened player's `/state` is reachable with the token and no
  cookie, that a wrong token and the no-credential case 401, and that the token does **not** open
  a different player's channel. Plus unit tests for the channel classification and the
  issue/hash round-trip.

Because it's additive and opt-in (`issue_token`), the server-initiated backends
(browser/MPD/Snapcast) are unaffected — they carry no token and are still covered by user auth.

## The native endpoint uses this

The **self-registering native endpoint** (`crates/musicata-endpoint`, the `native` player kind)
is built on exactly this: it registers with `issue_token`, then presents its scoped token on the
WS channel and on each stream fetch — holding no user account. See
[native-endpoint.md](native-endpoint.md).

## Still future work

The endpoint already **pins the server URL** it registered with (saved in its creds and the only
host it talks to). A stronger **server → endpoint** challenge (an optional server-issued nonce)
is specified but not built — on the LAN the pinned URL + scoped token suffice.

## What also shipped for M10

`PlayerCapabilities` is advertised **per player** off the `PlayerHandle` (like
`ProviderCapabilities` for sources) rather than hardcoded on the descriptor, so a controller can
ask what a backend supports (seek / volume / repeat / shuffle / queue editing). All current
backends are full-capability; the per-variant seam is where a future bridged endpoint
(Chromecast / UPnP / Squeezelite) declares a reduced set. See `GET /api/players`.
