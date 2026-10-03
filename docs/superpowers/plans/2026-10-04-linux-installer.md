# Linux installer implementation plan

Goal: install and upgrade Musicata on Ubuntu, Debian, Arch derivatives and Fedora, with Docker as the default and native systemd as an option.

User-approved scope (2026-10-04): report blockers and missing dependencies; offer Docker/native; ML only with Docker and compatible CPU instructions; install distro dependencies; support upgrades. Optional audio/discovery components follow the existing deployment conventions. Do not publish, change a live installation, or commit without a further request.

Implementation:
- [x] Add a Python standard-library installer and small shell bootstrap. Test distro/CPU detection, mode/component validation, plan-only behavior, and package mappings before implementation.
- [x] Implement artifact preparation, native systemd and Docker deployment with persistent state, non-root processes, and optional audio/discovery dependencies. Prepare downloads/images before stopping a working service.
- [x] Implement saved installation settings, explicit upgrades, stopped-service database backups, and recovery on failed activation. Refuse unmanaged/conflicting installations rather than overwrite them.
- [x] Include installer in release archives; document usage, package availability, private-registry diagnostics, CPU eligibility and recovery.
- [x] Verify unit tests, dry-run matrices and isolated install/upgrade flows; review changes and run relevant repository checks.

Verification focus: unsupported CPUs/architectures; unknown distributions; missing optional packages; interrupted/failed upgrades; preservation of database and custom settings; Docker/runtime package absence; tar path traversal and checksum mismatch; old manual installations; offline or private release registries.

UX work in the same branch: hide ordinary autoplay prefetch feedback in public playback snapshots, retain explicit-radio and end-of-queue wait feedback. Regression reproduced before the fix; all 355 Rust tests pass. Browser smoke verification passed.

Validation: 16 installer unit tests; native install/upgrade of the public 1.0.9 release in disposable Debian 13, Ubuntu 26.04, Fedora 44 and Arch Kubernetes pods. Test-only service-manager shims launch the real non-root server; host boot behavior is not covered. Docker integration passed on lizard10: real initial install, upgrade with persisted state, and recovery from an intentionally unhealthy replacement. GHCR access was replaced by a local image built from the public release binary; container execution and persistence were real. Existing ML remained running and test resources were removed. ShellCheck, rustfmt and diff whitespace checks passed; independent review found no critical/important issues.
