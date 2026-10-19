// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-disk cache of cover-art bytes.
//!
//! Local-disk covers are already fast to read, but a network source (e.g. SMB) would
//! otherwise be fetched over the wire on *every* request — slow, and it fails when the
//! source is offline. This caches the original bytes on first request, keyed by a
//! stable id of the source artwork, and serves them from disk thereafter.
//!
//! Lazy population only (no resizing or eviction yet) — see roadmap M3 / prior-art §8.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::fs;

/// Per-process sequence making each temp file name unique, so concurrent `put`s never share a
/// temp path (different extensions of the same key, or two writers of the same entry).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Retry unavailable/undecodable covers less often without permanently hiding
/// newly acquired artwork. Entries are pruned against the current album list.
#[derive(Default)]
pub struct PrewarmRetries {
    entries: std::collections::HashMap<String, (String, u32, std::time::Instant)>,
}
impl PrewarmRetries {
    pub fn ready(&self, id: &str, revision: &str, now: std::time::Instant) -> bool {
        self.entries
            .get(id)
            .is_none_or(|(old, _, next)| old != revision || now >= *next)
    }
    pub fn failed(&mut self, id: &str, revision: &str, now: std::time::Instant) {
        let count = self
            .entries
            .get(id)
            .filter(|(old, _, _)| old == revision)
            .map_or(0, |(_, count, _)| count.saturating_add(1));
        let delay = (300 * (1u64 << count.min(7))).min(6 * 60 * 60);
        self.entries.insert(
            id.into(),
            (
                revision.into(),
                count,
                now + std::time::Duration::from_secs(delay),
            ),
        );
    }
    pub fn succeeded(&mut self, id: &str) {
        self.entries.remove(id);
    }
    pub fn retain(&mut self, albums: &std::collections::HashSet<&str>) {
        self.entries.retain(|id, _| albums.contains(id.as_str()));
    }
}

/// A flat, sharded directory of cached artwork files.
#[derive(Clone)]
pub struct ArtworkCache {
    dir: PathBuf,
}

impl ArtworkCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `{dir}/{ab}/{key}.{ext}` — sharded by the first two key chars so one directory
    /// never holds the whole library's worth of files.
    fn entry_path(&self, key: &str, extension: &str) -> PathBuf {
        let shard = key.get(0..2).unwrap_or("__");
        let name = if extension.is_empty() {
            key.to_string()
        } else {
            format!("{key}.{extension}")
        };
        self.dir.join(shard).join(name)
    }

    /// Cached bytes for this key, or `None` on a miss (or any read error — a bad cache
    /// entry just means we re-fetch). Only network-backed sources populate the cache
    /// today, so this is unused in a build without any such provider.
    #[cfg_attr(not(feature = "provider-smb"), allow(dead_code))]
    pub async fn get(&self, key: &str, extension: &str) -> Option<Vec<u8>> {
        fs::read(self.entry_path(key, extension)).await.ok()
    }

    /// Store bytes for this key. Best-effort: writes to a temp file then atomically
    /// renames so a concurrent reader never sees a partial file; any failure is logged
    /// and ignored (the caller still serves the freshly-fetched bytes).
    #[cfg_attr(not(feature = "provider-smb"), allow(dead_code))]
    pub async fn put(&self, key: &str, extension: &str, bytes: &[u8]) {
        let path = self.entry_path(key, extension);
        let Some(parent) = path.parent() else { return };
        if let Err(error) = fs::create_dir_all(parent).await {
            tracing::warn!(%error, "artwork cache: create dir failed");
            return;
        }
        // A temp name unique per writer (key + extension + a process-wide sequence): different
        // extensions of the same key (an original and its resized variants) and two concurrent
        // writers must not share a temp, or one's rename removes the file the other is renaming
        // ("No such file or directory"), dropping that cache entry. The rename into `path` stays
        // atomic, so a reader never sees a partial file.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(
            ".{key}.{extension}.{}.{seq}.tmp",
            std::process::id()
        ));
        if let Err(error) = fs::write(&tmp, bytes).await {
            tracing::warn!(%error, "artwork cache: write failed");
            return;
        }
        if let Err(error) = fs::rename(&tmp, &path).await {
            tracing::warn!(%error, "artwork cache: rename failed");
            let _ = fs::remove_file(&tmp).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("musicata-artcache-{name}-{nanos}"))
    }

    #[test]
    fn failed_prewarming_backs_off_and_changed_artwork_retries_immediately() {
        let mut retries = PrewarmRetries::default();
        let now = std::time::Instant::now();
        assert!(retries.ready("album", "old-cover", now));
        retries.failed("album", "old-cover", now);
        assert!(!retries.ready(
            "album",
            "old-cover",
            now + std::time::Duration::from_secs(299)
        ));
        let next = now + std::time::Duration::from_secs(300);
        assert!(retries.ready("album", "old-cover", next));
        retries.failed("album", "old-cover", next);
        assert!(!retries.ready(
            "album",
            "old-cover",
            next + std::time::Duration::from_secs(599)
        ));
        assert!(retries.ready(
            "album",
            "old-cover",
            next + std::time::Duration::from_secs(600)
        ));
        assert!(retries.ready("album", "new-cover", next));
        retries.failed("album", "new-cover", next);
        assert!(retries.ready(
            "album",
            "new-cover",
            next + std::time::Duration::from_secs(300)
        ));
        retries.succeeded("album");
        assert!(retries.ready("album", "new-cover", next));
        for _ in 0..100 {
            retries.failed("album", "new-cover", now);
        }
        assert!(!retries.ready(
            "album",
            "new-cover",
            now + std::time::Duration::from_secs(21599)
        ));
        assert!(retries.ready(
            "album",
            "new-cover",
            now + std::time::Duration::from_secs(21600)
        ));
    }

    #[tokio::test]
    async fn put_then_get_round_trips_and_shards() {
        let dir = temp_dir("roundtrip");
        let cache = ArtworkCache::new(&dir);

        assert!(cache.get("abcdef", "jpg").await.is_none(), "cold miss");

        cache.put("abcdef", "jpg", b"image-bytes").await;
        assert_eq!(
            cache.get("abcdef", "jpg").await.as_deref(),
            Some(&b"image-bytes"[..])
        );
        // Sharded by the first two key chars.
        assert!(dir.join("ab").join("abcdef.jpg").exists());

        // A different extension is a different entry.
        assert!(cache.get("abcdef", "png").await.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_puts_of_same_key_all_persist() {
        // Regression: the original + resized variants of one cover share a key but differ by
        // extension; a shared temp name made concurrent renames race and drop entries. Every
        // distinct (key, extension) must survive a concurrent storm.
        let dir = temp_dir("concurrent");
        let cache = ArtworkCache::new(&dir);
        let exts = ["jpg", "128.jpg", "300.jpg", "600.jpg", "embedded"];

        let mut tasks = Vec::new();
        for ext in exts {
            // Several writers per (key, ext) too, to exercise same-entry races.
            for _ in 0..4 {
                let cache = cache.clone();
                tasks.push(tokio::spawn(async move {
                    cache
                        .put("abcdef", ext, format!("bytes-{ext}").as_bytes())
                        .await;
                }));
            }
        }
        for task in tasks {
            task.await.unwrap();
        }

        for ext in exts {
            assert_eq!(
                cache.get("abcdef", ext).await.as_deref(),
                Some(format!("bytes-{ext}").as_bytes()),
                "entry {ext} should persist after concurrent puts",
            );
        }
        // No stray temp files left behind.
        let leftover: Vec<_> = std::fs::read_dir(dir.join("ab"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "temp files left: {leftover:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
