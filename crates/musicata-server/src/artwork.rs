//! On-disk cache of cover-art bytes.
//!
//! Local-disk covers are already fast to read, but a network source (e.g. SMB) would
//! otherwise be fetched over the wire on *every* request — slow, and it fails when the
//! source is offline. This caches the original bytes on first request, keyed by a
//! stable id of the source artwork, and serves them from disk thereafter.
//!
//! Lazy population only (no resizing or eviction yet) — see roadmap M3 / prior-art §8.

use std::path::PathBuf;

use tokio::fs;

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
        // Concurrent puts of the same key write identical bytes, so sharing the temp
        // name is harmless and the rename is atomic.
        let tmp = parent.join(format!(".{key}.tmp"));
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
}
