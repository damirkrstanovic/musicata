# Player endpoint authentication — design (M10)

Status: **designed, enforcement deferred** (2026-06-27). See `decisions.md`.

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

## Why enforcement is deferred

There is nothing to present a token yet: every current backend is server-initiated and already
covered by user auth. Adding the column + issuance now, with no client that sends the token,
would be unenforced scaffolding — security theatre that still has to be revisited when the
native endpoint lands. So the **mechanism is specified here** and ships **with** the native
endpoint prototype (the same M10 task), as one coherent change with a real client on the other
end and tests that exercise a rejected/forged token.

## What did ship for M10 now

`PlayerCapabilities` is advertised **per player** off the `PlayerHandle` (like
`ProviderCapabilities` for sources) rather than hardcoded on the descriptor, so a controller can
ask what a backend supports (seek / volume / repeat / shuffle / queue editing). All current
backends are full-capability; the per-variant seam is where a future bridged endpoint
(Chromecast / UPnP / Squeezelite) declares a reduced set. See `GET /api/players`.
