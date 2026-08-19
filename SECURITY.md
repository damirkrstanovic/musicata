# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub's
[private vulnerability reporting](https://github.com/damirkrstanovic/musicata/security/advisories/new)
("Security" tab → "Report a vulnerability"). That opens a draft advisory only you and the
maintainers can see.

Useful things to include: the affected version (`musicata-server --version` or the release
tag), what an attacker can do, and a concrete repro — a request, a file, or a few lines of
code. Reports that show the failing path get fixed fastest.

Musicata is a single-maintainer hobby project, so there is no paid triage rotation and no
bounty. Expect a first response within about a week. Once a fix ships, the advisory is
published with credit to the reporter unless you ask otherwise.

## Supported versions

Only the latest release line receives fixes.

| Version | Supported |
| ------- | --------- |
| 1.0.x   | ✅        |
| < 1.0   | ❌        |

Releases before 1.0 predate the [2026-06-27 source audit](docs/audit-findings.md) and contain
known, now-fixed security bugs. Do not run them.

## Threat model — read this before deploying

Musicata is **LAN-first by design**. It assumes it runs on a network you trust, reached by
people you trust. Concretely, in the current release:

- **Session cookies are not marked `Secure`.** Musicata serves plain HTTP; put it behind a TLS
  reverse proxy if you need encryption in transit.
- **Music-source credentials are stored in clear text** in the SQLite database — SMB, MPD and
  upstream OpenSubsonic passwords, plus per-user API tokens. Anyone who can read
  `musicata.db` can read them. Keep the data directory to the service user and back it up
  accordingly.
- **There is no rate limiting or account lockout** on login beyond constant-time password
  verification and a password-length cap.

Do not expose Musicata directly to the internet. Reach it from outside over a VPN
(Tailscale/WireGuard) or an SSH tunnel — see
[Remote access](docs/deployment.md#remote-access).

Findings that amount to "an untrusted user on the LAN can do X" are in scope. Findings that
depend on a deployment the documentation explicitly warns against — Musicata published raw to
the internet — are known and documented rather than separate vulnerabilities, though a report
that makes such a deployment meaningfully safer is still welcome.
