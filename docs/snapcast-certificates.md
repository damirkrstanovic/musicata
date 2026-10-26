# Snapcast certificate authentication investigation

Research and isolated PoC, 2026-10-26. Implementation plan and progress:
[issue #72](https://github.com/damirkrstanovic/musicata/issues/72).
**Status: deferred / maybe.** The user chose trusted-network operation over implementing
certificate lifecycle management. The proposal below is retained as research, not an active
implementation plan or a description of shipped protection.

## Recommendation

Musicata should own enrollment, issuance, renewal and revocation. Keep stock Snapclient
as the audio renderer. Put an audio-only mutual-TLS gateway inside Musicata's Rust server,
with Snapserver reachable only on loopback. The listener supplies the server address and
one-use setup invitation; certificates remain an installer/service implementation detail.
This does not restore the removed native audio endpoint or require a separate Python daemon.

Native Snapserver mTLS is useful transport protection but does not implement the player
identity and permissions Musicata needs. A generic TLS reverse proxy alone also misses these
checks. Forking Snapcast would add an ongoing maintenance burden; an application gateway
lets Musicata enforce its own identity registry and immediately revoke individual sessions.
The cost is another audio relay, whose latency, buffering and synchronization need measurement.

## Upstream findings

Checked upstream v0.35.0, revision f1237347, and tested its release binaries:

| Observation | Evidence / consequence |
|---|---|
| Native mTLS rejects absent, foreign, expired and wrong-purpose client certificates. | Real WSS handshakes in the PoC. |
| A valid player certificate can use `/jsonrpc`. | `Server.GetStatus` succeeded. Certificate admission is not an audio-only permission. |
| A client certificate can claim an unrelated Hello ID. | Stock Snapclient rendered audio with another `--hostID`. |
| Stock Snapclient accepts a server SAN mismatch. | Connected to 127.0.0.1 using a certificate whose only SAN was DNS:localhost. |
| `--server-cert` adds trust; it does not remove default roots. | Explicit unrelated CA plus a valid default root still connected. |
| Restricting OpenSSL default roots works in the tested Linux build. | `SSL_CERT_FILE` set to the instance CA and `SSL_CERT_DIR` to an empty private directory; intended server worked, unrelated trust failed. |

Source: [native TLS admission](https://github.com/snapcast/snapcast/blob/v0.35.0/server/control_server.cpp),
[client verification](https://github.com/snapcast/snapcast/blob/v0.35.0/client/client_connection.cpp),
[client trust configuration](https://github.com/snapcast/snapcast/blob/v0.35.0/client/controller.cpp),
[Hello identity handling](https://github.com/snapcast/snapcast/blob/v0.35.0/server/server.cpp).
The release still disables its separate username/password authorization framework in
[snapserver.cpp](https://github.com/snapcast/snapcast/blob/v0.35.0/server/snapserver.cpp).
A future upstream change must be tested; existing password configuration is not a guarantee
that authorization will automatically become correct or compatible.

Native server verification also loads default trust roots. The proposed Rust gateway must
trust only Musicata's client issuer, not the system/public CA collection.

## Proposed lifecycle and trust model

These are proposed defaults, not requirements imposed by Snapcast:

- One private server-trust CA and a separate client-issuing CA per Musicata installation.
  The server CA issues certificates only for that installation's server. Store private keys
  in its persistent state directory, owner-only, with atomic writes. Store device ID,
  certificate serial/fingerprint, validity and revocation metadata in SQLite. Never return
  CA keys through APIs or logs. Use random 128-bit certificate serials.
- The Add player wizard produces a 256-bit, single-use invitation valid for 10 minutes.
  Include the public server-CA fingerprint in the invitation. The installer obtains the
  invitation via a private prompt/file, not an argument retained in shell history.
  It checks that fingerprint before transmitting the secret over verified TLS. This assumes
  the invitation came from the user's trusted Musicata UI/channel; unauthenticated CA download
  is not bootstrap trust. A valid HTTPS administration origin remains preferable.
- Generate the device key on the device. Send only its CSR. Musicata verifies proof of key
  possession and assigns an immutable player ID in a clientAuth URI SAN, ignoring requested
  identity/privilege extensions. Consume invitations atomically; retrying the same CSR can
  retrieve its public certificate, but cannot enroll a different key with the consumed token.
- Use 365-day client certificates; begin automatic renewal 90 days before expiry, with jitter
  and bounded retry. Authenticate renewal with the existing certificate through a restricted
  renewal route. Reject revoked/disabled devices. Allow old/new certificates to overlap until
  a new connection succeeds, with a 24-hour upper bound, then close the old session.
- Enforce admission using issuer, EKU, validity, serial/fingerprint registry and player ID.
  Bind the Snapcast Hello ID to that identity and permit only the required audio protocol
  messages. Reject control RPC on every gateway path. Only one active audio session per player;
  replacement during renewal explicitly supersedes the old session.
- Revocation durably records denial before acknowledging success, rejects reconnects and
  closes live sessions. Schedule session closure at certificate expiry: handshake validation
  alone does not expire an established connection. Playback reads authorization from memory;
  database writes must not stall the audio path.
- Server leaves last 90 days, renewing before 30 days remain. Roots last 10 years with planned
  overlapping rotation at least a year before expiry. Distribute replacement roots over the
  existing authenticated connection, authenticated by the old trust. Clients offline past
  the overlap, or with lost/expired credentials, use a new invitation to repair the same player
  identity without losing its zone. Root rotation and rollback must have integration tests.
- Managed Linux clients restrict both explicit and default trust to the instance server CA.
  Because stock Snapclient does not verify hostnames, this is instance identity via a dedicated
  trust anchor, not standard DNS identity verification. Validate every supported package/TLS
  backend; do not assume the tested environment-variable behavior is portable. Unsupported
  clients require an explicit compatibility mode or a verified upstream client fix.
- Back up database and issuer keys together using the existing protected backup mechanism.
  Losing keys requires re-pairing; restoring old revocation state must enter recovery mode
  and require re-enrollment rather than silently resurrecting revoked credentials.
  CA compromise requires replacing trust and re-pairing. Do not add Vault or an external PKI
  service just for a home installation.

One invitation can provision playback and optional device management in the same user flow,
but their credentials/scopes remain separate. A playback certificate must not gain power,
installer, or administration privileges. Renewal must also work on players without USB
standby/device-management support.

## Measured PoC

Script: `tests/snapcast/cert_auth_poc.py`. Report: `tests/snapcast/cert-auth-poc-result.json`.
Cluster context `homelab-k3s`, namespace `musicata-build`, pod `musicata-health64`;
working directory `/work/cert-poc`. Upstream Snapserver and Snapclient 0.35.0 were extracted
privately, without replacing installed binaries. All listeners were loopback-only.

**26 checks passed.** Two stock Snapclients rendered synthetic PCM through the experimental
mTLS gateway. It rejected unknown/revoked certificates, identity spoofing and RPC access.
Replacing a certificate restored playback after revocation. Revocation closed the old
session in **102 ms** in this run; the second player's audio delivery continued. This is
one bounded observation, not a latency guarantee or proof of uninterrupted physical sound.
Temporary keys and processes were confirmed removed after the run. Minipc was unchanged.

The experiment does **not** implement invitation redemption, an enrollment HTTP API,
durable state, automatic renewal scheduling, production protocol hardening, or the Rust
integration. It does not measure multi-zone synchronization, real DAC sound, USB wake,
resource overhead or long-running network faults. Replacement issuance/admission was tested;
automatic renewal was not.

### Reproduction

Use an isolated Kubernetes pod with Python 3.11, OpenSSL and the upstream 0.35.0 binaries.
Install `websockets==15.0.1` into a private dependency directory; it is a PoC-only dependency
(BSD-3-Clause), not a Musicata runtime dependency. The tested wheel SHA256 is
`f7a866fbc1e97b5c617ee4116daaa09b722101d4a3c170c787450ba409f9736f`.

```sh
PYTHONPATH=/work/cert-poc/deps python3 tests/snapcast/cert_auth_poc.py \
  --snapserver /work/cert-poc/upstream/usr/bin/snapserver \
  --snapclient /work/cert-poc/upstream/usr/bin/snapclient \
  --report /work/cert-poc/result.json
```

Upstream amd64 bookworm package SHA256s:

- snapserver: `afb012318bb8cbf82c63c2084de3b290f53108c49976ff364100882cd8816b41`
- snapclient: `b71575bcc2b4541f5004c73a0a1f43374e852bf5d5f2162aa1b230f3c209d9f3`

The PoC's `--server-cert=<path>` uses an equals sign deliberately: the upstream option has
an optional argument. It connects stock clients over IPv4 to avoid an observed localhost
IPv6-first connection failure. Neither workaround should conceal supported-platform testing.
