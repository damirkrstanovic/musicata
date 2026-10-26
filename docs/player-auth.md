# Player authentication

Musicata can issue an opt-in, per-player bearer token when an MPD player is registered with
`"issue_token": true`. The registration response contains the random token once; the database
stores only its SHA-256 hash. A player can present its own token on its state, command, and
playback-state WebSocket channels, and when fetching library streams, without holding a user
account. A token is scoped to that player and cannot manage players, edit settings, or access
another player's channels.

User sessions still authorize the same channels and are required for player management. Snapcast
is server-managed, and browser playback uses the authenticated browser session.

The browser renderer additionally obtains a connection-scoped lease on
`/api/players/browser-local/audio/ws`. That private capability confines browser audio
configuration, meter reports, and diagnostics to the tab currently rendering audio. It is not a
credential for player management or other outputs.

## Retired native endpoint

The self-registering native playback endpoint and its `native` player kind were removed on
2026-10-26 in favour of browser playback, MPD, and Snapcast. Existing databases may retain old
native player rows and token hashes for migration compatibility. Those native rows are inert and
their hashes do not authorize any request; a user can remove a legacy row through normal player
management. The scoped-token protocol remains available to supported MPD players.
