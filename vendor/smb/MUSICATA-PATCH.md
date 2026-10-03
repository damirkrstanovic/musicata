# Musicata's SMB directory wake-up patch

This is the published `smb` 0.11.2 crate from crates.io (MIT), whose upstream commit
is `46981b82d2327601d606adf2e4d1f27ec7857ad3` in
<https://github.com/afiffon/smb-rs>. The upstream LICENSE.md from that commit is
included here because the crate archive omitted it.

Runtime change: `QueryDirectoryStream::poll_next` uses `notify_one()` instead of
`notify_waiters()` when the receiver drains a batch. The producer can be preempted
after sending its last entry, before awaiting `notified()`. `notify_waiters()` loses
a notification in that interval; `notify_one()` retains a permit for the producer.
No public API, SMB protocol, or scanner concurrency setting changes.

The colocated `batch_wakeup_tests` exercise the actual stream's `poll_next` with
both early and already-registered producer waits, buffered entries and stream EOF.
They need no NAS or credentials. Run them from the repository root:

```sh
cargo test --manifest-path vendor/smb/Cargo.toml --target-dir target --locked --lib batch_wakeup_tests
```

The standalone test lockfile starts from Musicata's resolved dependencies, with
upstream test-only dependencies added. The root Cargo.lock controls server builds.
Other than these tests, this note, the license and test lockfile, keep the crate
identical to the published archive. Remove the Cargo patch and vendored copy once
a released upstream version contains the fix and passes these regressions.

Live validation: a read-only four-connection parallel directory probe on a Celeron
N4500 stalled after 150 directories before this change. With only the notification
changed, the same NAS yielded 1,189 directories / 14,487 files in 4.85 and 4.98 seconds.
