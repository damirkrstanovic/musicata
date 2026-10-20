// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-disk cache of cover-art bytes.
//!
//! Local-disk covers are already fast to read, but a network source (e.g. SMB) would
//! otherwise be fetched over the wire on *every* request — slow, and it fails when the
//! source is offline. This caches the original bytes on first request, keyed by a
//! stable id of the source artwork, and serves them from disk thereafter.
//!
//! Source aliases retain provenance; content objects deduplicate originals and thumbnails.
//! Independent maintenance bounds disposable cache bytes while preserving manual pins.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncReadExt;

/// Match the existing SMB cover limit; audio recordings are read through a stream.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

pub async fn read_image_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    if len > MAX_IMAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artwork exceeds the byte limit",
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artwork exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

/// Per-process sequence making each temp file name unique, so concurrent `put`s never share a
/// temp path (different extensions of the same key, or two writers of the same entry).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
const ALIAS_MAGIC: &str = "musicata-artwork-v1";
const PIN_GC_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Clone, Debug)]
struct Alias {
    key: String,
    extension: String,
    hash: String,
    len: u64,
    last_access: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObjectStamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct ArtworkCacheMaintenance {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub pinned_bytes: u64,
    pub evicted_objects: usize,
    pub removed_pins: usize,
}

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
    pub fn pending(&self) -> (usize, Option<std::time::Instant>) {
        (
            self.entries.len(),
            self.entries.values().map(|(_, _, next)| *next).min(),
        )
    }
}

/// A flat, sharded directory of cached artwork files.
#[derive(Clone)]
pub struct ArtworkCache {
    dir: PathBuf,
    processing: Arc<tokio::sync::Semaphore>,
    mutation: Arc<tokio::sync::RwLock<()>>,
    recent_pins: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    verified: Arc<Mutex<HashMap<String, ObjectStamp>>>,
    accesses: Arc<Mutex<HashMap<String, u64>>>,
    legacy_failures: Arc<Mutex<HashMap<PathBuf, std::time::Instant>>>,
    source_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
}

pub type ProcessingPermit = std::sync::Arc<tokio::sync::OwnedSemaphorePermit>;

impl ArtworkCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            processing: Arc::new(tokio::sync::Semaphore::new(3)),
            mutation: Arc::new(tokio::sync::RwLock::new(())),
            recent_pins: Arc::default(),
            verified: Arc::default(),
            accesses: Arc::default(),
            legacy_failures: Arc::default(),
            source_locks: Arc::default(),
        }
    }

    /// Shared by requests and prewarming; clones held by blocking jobs keep the
    /// allocation bound in force even if their HTTP request is cancelled.
    pub async fn processing_permit(&self) -> ProcessingPermit {
        Arc::new(
            self.processing
                .clone()
                .acquire_owned()
                .await
                .expect("artwork semaphore is never closed"),
        )
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

    fn pin_path(&self, key: &str, extension: &str) -> PathBuf {
        let shard = key.get(0..2).unwrap_or("__");
        let name = if extension.is_empty() {
            key.to_string()
        } else {
            format!("{key}.{extension}")
        };
        self.dir.join("pinned").join(shard).join(name)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(hash)
    }

    /// Serializes a source miss, its refetch, and the caller's second cache lookup without
    /// making unrelated artwork requests wait. Callers should obtain this before their
    /// miss/double-check/fetch sequence.
    pub async fn source_lock(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.source_locks.lock().expect("source locks poisoned");
            locks.retain(|_, weak| weak.strong_count() > 0);
            locks
                .entry(id.to_owned())
                .or_insert_with(|| Arc::downgrade(&Arc::new(tokio::sync::Mutex::new(()))))
                .upgrade()
                .unwrap_or_else(|| {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(id.to_owned(), Arc::downgrade(&lock));
                    lock
                })
        };
        lock.lock_owned().await
    }

    /// Cached bytes for this key, or `None` on a miss (or any read error — a bad cache
    /// entry just means we re-fetch). Only network-backed sources populate the cache
    /// today, so this is unused in a build without any such provider.
    #[cfg_attr(not(feature = "provider-smb"), allow(dead_code))]
    pub async fn get(&self, key: &str, extension: &str) -> Option<Vec<u8>> {
        self.get_at(&self.entry_path(key, extension), false).await
    }

    /// Store bytes for this key. Best-effort: writes to a temp file then atomically
    /// renames so a concurrent reader never sees a partial file; any failure is logged
    /// and ignored (the caller still serves the freshly-fetched bytes).
    #[cfg_attr(not(feature = "provider-smb"), allow(dead_code))]
    pub async fn put(&self, key: &str, extension: &str, bytes: &[u8]) {
        if let Err(error) = self
            .store_at(&self.entry_path(key, extension), key, extension, bytes)
            .await
        {
            tracing::warn!(%error, "artwork cache: write failed");
        }
    }

    /// Persist user-selected artwork separately from ordinary cache aliases. The caller stores
    /// the selection in the DB after this succeeds, so a DB choice never points at absent data.
    pub async fn put_pinned(&self, key: &str, extension: &str, bytes: &[u8]) -> io::Result<()> {
        self.store_at(&self.pin_path(key, extension), key, extension, bytes)
            .await
    }

    pub async fn get_pinned(&self, key: &str, extension: &str) -> Option<Vec<u8>> {
        self.get_at(&self.pin_path(key, extension), true).await
    }

    #[cfg(test)]
    async fn delete_pinned(&self, key: &str, extension: &str) -> bool {
        match fs::remove_file(self.pin_path(key, extension)).await {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!(%error, "artwork cache: remove pin failed");
                false
            }
        }
    }

    /// Removes every ordinary cached variant for a source key. Hold `source_lock(key)` across
    /// this call and the following refetch so a pre-refresh fetch cannot write stale bytes late.
    /// Pinned/manual aliases are intentionally outside this operation.
    pub async fn invalidate(&self, key: &str) -> io::Result<usize> {
        let mut aliases = Vec::new();
        collect_aliases(&self.dir, false, &mut aliases, &mut Vec::new()).await?;
        let mut removed = 0;
        for (path, alias, _) in aliases {
            if alias.key == key && fs::remove_file(path).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    async fn get_at(&self, path: &Path, pinned: bool) -> Option<Vec<u8>> {
        let bytes = read_image_file(path).await.ok()?;
        match parse_alias(&bytes) {
            Ok(alias) => {
                let object = self.object_path(&alias.hash);
                let bytes = read_image_file(&object).await.ok()?;
                if bytes.len() as u64 != alias.len
                    || !self.object_is_verified(&alias.hash, &object, &bytes).await
                {
                    let _ = fs::remove_file(&object).await;
                    let _ = fs::remove_file(path).await;
                    return None;
                }
                self.touch(&alias.hash);
                Some(bytes)
            }
            Err(()) if pinned => {
                let _ = fs::remove_file(path).await;
                None
            }
            // Legacy entries are decoded once and atomically migrated to an alias. A malformed
            // old entry is removed; current entries never need a decoder on their hot path.
            Err(()) => {
                if self
                    .legacy_failures
                    .lock()
                    .expect("legacy failures poisoned")
                    .get(path)
                    .is_some_and(|retry| std::time::Instant::now() < *retry)
                {
                    return None;
                }
                // A caller that already owns one of the processing permits must not deadlock
                // trying to validate an old cache entry. It can regenerate the miss instead.
                let Ok(permit) = self.processing.clone().try_acquire_owned() else {
                    return None;
                };
                let decode_bytes = bytes.clone();
                let valid = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    legacy_image_is_valid(&decode_bytes)
                })
                .await
                .unwrap_or(false);
                if !valid {
                    let _ = fs::remove_file(path).await;
                    self.legacy_failures
                        .lock()
                        .expect("legacy failures poisoned")
                        .remove(path);
                    return None;
                }
                let (key, extension) = path_key(path)?;
                if self.store_at(path, &key, &extension, &bytes).await.is_err() {
                    self.legacy_failures
                        .lock()
                        .expect("legacy failures poisoned")
                        .insert(
                            path.to_owned(),
                            std::time::Instant::now() + std::time::Duration::from_secs(300),
                        );
                    return None;
                }
                self.legacy_failures
                    .lock()
                    .expect("legacy failures poisoned")
                    .remove(path);
                Some(bytes)
            }
        }
    }

    async fn object_is_verified(&self, hash: &str, object: &Path, bytes: &[u8]) -> bool {
        let metadata = match fs::metadata(object).await {
            Ok(metadata) => metadata,
            Err(_) => return false,
        };
        let stamp = ObjectStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        if self
            .verified
            .lock()
            .expect("verified cache poisoned")
            .get(hash)
            == Some(&stamp)
        {
            return true;
        }
        let valid = sha256(bytes) == hash;
        if valid {
            self.verified
                .lock()
                .expect("verified cache poisoned")
                .insert(hash.to_owned(), stamp);
        }
        valid
    }

    fn touch(&self, hash: &str) {
        let sequence = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.accesses
            .lock()
            .expect("accesses poisoned")
            .insert(hash.to_owned(), sequence);
    }

    async fn store_at(
        &self,
        path: &Path,
        key: &str,
        extension: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let _mutation = self.mutation.read().await;
        if bytes.len() as u64 > MAX_IMAGE_BYTES
            || !safe_component(key)
            || !safe_extension(extension)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid artwork cache entry",
            ));
        }
        let hash = sha256(bytes);
        let object = self.object_path(&hash);
        let needs_write = match read_image_file(&object).await {
            Ok(existing) => existing != bytes,
            Err(_) => true,
        };
        if needs_write {
            write_atomic(&object, bytes).await?;
        }
        let metadata = fs::metadata(&object).await?;
        self.verified
            .lock()
            .expect("verified cache poisoned")
            .insert(
                hash.clone(),
                ObjectStamp {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            );
        let alias = Alias {
            key: key.to_owned(),
            extension: extension.to_owned(),
            hash: hash.clone(),
            len: bytes.len() as u64,
            last_access: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        write_atomic(path, &encode_alias(&alias)).await?;
        if path.starts_with(self.dir.join("pinned")) {
            self.recent_pins
                .lock()
                .expect("pins poisoned")
                .insert(hash.clone(), std::time::Instant::now());
        }
        self.touch(&hash);
        Ok(())
    }

    /// Reclaims unpinned content objects down to `max_bytes`. `pinned` is the DB's live set of
    /// manual artwork keys; stale pinned aliases are removed here, while live pins are never
    /// evicted even if they alone exceed the requested budget.
    pub async fn maintain(
        &self,
        max_bytes: u64,
        pinned: &HashSet<String>,
    ) -> io::Result<ArtworkCacheMaintenance> {
        let mut report = ArtworkCacheMaintenance::default();
        let mut aliases = Vec::new();
        let mut legacy = Vec::new();
        collect_aliases(&self.dir, false, &mut aliases, &mut legacy).await?;
        let mut pin_aliases = Vec::new();
        collect_aliases(
            &self.dir.join("pinned"),
            true,
            &mut pin_aliases,
            &mut Vec::new(),
        )
        .await?;
        let mut live_hashes = HashSet::new();
        for (path, alias, _) in pin_aliases {
            let recent = fs::metadata(&path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age < PIN_GC_GRACE);
            if pinned.contains(&alias.key) || recent {
                live_hashes.insert(alias.hash);
            } else {
                let _mutation = self.mutation.write().await;
                let still_stale = fs::metadata(&path)
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= PIN_GC_GRACE);
                if still_stale && fs::remove_file(path).await.is_ok() {
                    report.removed_pins += 1;
                }
            }
        }
        let mut objects = read_objects(&self.dir.join("objects")).await?;
        let mut alias_bytes = HashMap::<String, u64>::new();
        for (path, alias, _) in &aliases {
            let bytes = fs::metadata(path)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            *alias_bytes.entry(alias.hash.clone()).or_default() += bytes;
        }
        report.bytes_before = objects.iter().map(|(_, _, len)| *len).sum::<u64>()
            + alias_bytes.values().sum::<u64>()
            + legacy.iter().map(|(_, len)| *len).sum::<u64>();
        report.pinned_bytes = objects
            .iter()
            .filter(|(hash, _, _)| live_hashes.contains(hash))
            .map(|(_, _, len)| *len)
            .sum();
        // Coalesce hot-path accesses into alias metadata only during maintenance. This makes the
        // next process retain a useful LRU ordering without turning every GET into a write.
        let accesses = self.accesses.lock().expect("accesses poisoned").clone();
        for (path, alias, _) in &mut aliases {
            if let Some(last_access) = accesses
                .get(&alias.hash)
                .filter(|value| **value > alias.last_access)
            {
                let _mutation = self.mutation.write().await;
                let current = read_image_file(path)
                    .await
                    .ok()
                    .and_then(|bytes| parse_alias(&bytes).ok());
                if current.as_ref().is_some_and(|current| {
                    current.hash == alias.hash && current.last_access < *last_access
                }) {
                    alias.last_access = *last_access;
                    let _ = write_atomic(path, &encode_alias(alias)).await;
                }
            }
        }
        let mut access_by_hash: HashMap<String, u64> = HashMap::new();
        for (_, alias, _) in &aliases {
            access_by_hash
                .entry(alias.hash.clone())
                .and_modify(|seen| *seen = (*seen).max(alias.last_access))
                .or_insert(alias.last_access);
        }
        objects.sort_by_key(|(hash, modified, _)| {
            (access_by_hash.get(hash).copied().unwrap_or(0), *modified)
        });
        // The budget deliberately applies only to automatic artwork. Manual selections are a
        // separate user-owned pool and therefore cannot force ordinary entries out early.
        let mut automatic_total = objects
            .iter()
            .filter(|(hash, _, _)| !live_hashes.contains(hash))
            .map(|(_, _, len)| *len)
            .sum::<u64>()
            .saturating_add(alias_bytes.values().sum())
            .saturating_add(legacy.iter().map(|(_, len)| *len).sum::<u64>());
        for (path, len) in legacy {
            if automatic_total <= max_bytes {
                break;
            }
            let _mutation = self.mutation.write().await;
            // A legacy file may have migrated since the scan; don't remove its new alias.
            if read_alias_file(&path)
                .await
                .ok()
                .is_some_and(|b| parse_alias(&b).is_err())
                && fs::remove_file(path).await.is_ok()
            {
                automatic_total = automatic_total.saturating_sub(len);
            }
        }
        for (hash, _, len) in objects {
            if automatic_total <= max_bytes || live_hashes.contains(&hash) {
                continue;
            }
            let object = self.object_path(&hash);
            let _mutation = self.mutation.write().await;
            // A pin may have been published after the maintenance snapshot. Its writer
            // records protection while holding the mutation read guard, before returning.
            if self
                .recent_pins
                .lock()
                .expect("pins poisoned")
                .get(&hash)
                .is_some_and(|at| at.elapsed() < PIN_GC_GRACE)
            {
                continue;
            }
            if fs::remove_file(&object).await.is_ok() {
                automatic_total = automatic_total
                    .saturating_sub(len.saturating_add(*alias_bytes.get(&hash).unwrap_or(&0)));
                report.evicted_objects += 1;
                self.verified
                    .lock()
                    .expect("verified cache poisoned")
                    .remove(&hash);
                self.accesses
                    .lock()
                    .expect("accesses poisoned")
                    .remove(&hash);
            }
        }
        // Pinned bytes are exempt, but ordinary aliases to those bytes are still automatic
        // cache storage and must not make the cache unbounded.
        let mut pinned_object_aliases: Vec<_> = aliases
            .iter()
            .filter(|(_, alias, _)| live_hashes.contains(&alias.hash))
            .collect();
        pinned_object_aliases.sort_by_key(|(_, alias, _)| alias.last_access);
        for (path, _, _) in pinned_object_aliases {
            if automatic_total <= max_bytes {
                break;
            }
            let _mutation = self.mutation.write().await;
            if let Ok(metadata) = fs::metadata(path).await {
                if fs::remove_file(path).await.is_ok() {
                    automatic_total = automatic_total.saturating_sub(metadata.len());
                }
            }
        }
        // Delete aliases after the scan, so maintenance never holds a global lock while walking
        // the cache. Concurrent readers may see a miss and refill; atomic rename keeps them safe.
        for (path, alias, _) in aliases {
            let _mutation = self.mutation.write().await;
            let current = read_alias_file(&path)
                .await
                .ok()
                .and_then(|b| parse_alias(&b).ok());
            if current.is_some_and(|a| a.hash == alias.hash)
                && fs::metadata(self.object_path(&alias.hash)).await.is_err()
            {
                let _ = fs::remove_file(path).await;
            }
        }
        self.recent_pins
            .lock()
            .expect("pins poisoned")
            .retain(|_, at| at.elapsed() < PIN_GC_GRACE);
        report.bytes_after = automatic_total.saturating_add(report.pinned_bytes);
        Ok(report)
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty() && safe_extension(value)
}
fn safe_extension(value: &str) -> bool {
    !value.contains(['/', '\\', '\n', '\r']) && value != "." && value != ".."
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn encode_alias(alias: &Alias) -> Vec<u8> {
    format!(
        "{ALIAS_MAGIC}\n{}\n{}\n{}\n{}\n{}\n",
        alias.key, alias.extension, alias.hash, alias.len, alias.last_access
    )
    .into_bytes()
}
fn parse_alias(bytes: &[u8]) -> Result<Alias, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut fields = text.split_terminator('\n');
    if fields.next() != Some(ALIAS_MAGIC) {
        return Err(());
    }
    let alias = Alias {
        key: fields.next().ok_or(())?.to_owned(),
        extension: fields.next().ok_or(())?.to_owned(),
        hash: fields.next().ok_or(())?.to_owned(),
        len: fields.next().ok_or(())?.parse().map_err(|_| ())?,
        last_access: fields.next().map_or(Ok(0), str::parse).map_err(|_| ())?,
    };
    if !safe_component(&alias.key)
        || !safe_extension(&alias.extension)
        || alias.hash.len() != 64
        || !alias.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || fields.next().is_some()
    {
        return Err(());
    }
    Ok(alias)
}
async fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"))?;
    fs::create_dir_all(parent).await?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.{}.{seq}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artwork"),
        std::process::id()
    ));
    fs::write(&tmp, bytes).await?;
    if let Err(error) = fs::rename(&tmp, path).await {
        let _ = fs::remove_file(&tmp).await;
        return Err(error);
    }
    Ok(())
}
fn legacy_image_is_valid(bytes: &[u8]) -> bool {
    let mut reader =
        match image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format() {
            Ok(reader) => reader,
            Err(_) => return false,
        };
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().is_ok()
}
fn path_key(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    let (key, extension) = name.split_once('.').unwrap_or((name, ""));
    Some((key.to_owned(), extension.to_owned()))
}
async fn collect_aliases(
    root: &Path,
    pinned: bool,
    output: &mut Vec<(PathBuf, Alias, bool)>,
    legacy: &mut Vec<(PathBuf, u64)>,
) -> io::Result<()> {
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let kind = match entry.file_type().await {
                Ok(kind) => kind,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if kind.is_dir() {
                if pinned || !matches!(entry.file_name().to_str(), Some("pinned") | Some("objects"))
                {
                    dirs.push(entry.path());
                }
            } else if kind.is_file() {
                let path = entry.path();
                // Atomic publication may rename a temporary entry after read_dir saw it.
                let len = match entry.metadata().await {
                    Ok(metadata) => metadata.len(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if len > MAX_IMAGE_BYTES {
                    if !pinned {
                        let _ = fs::remove_file(path).await;
                    }
                    continue;
                }
                match read_alias_file(&path).await {
                    Ok(bytes) => {
                        if let Ok(alias) = parse_alias(&bytes) {
                            output.push((path, alias, pinned));
                        } else if !pinned {
                            legacy.push((path, len));
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
    Ok(())
}

async fn read_alias_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path).await?;
    let mut bytes = Vec::with_capacity(1025);
    file.take(1025).read_to_end(&mut bytes).await?;
    Ok(bytes)
}
async fn read_objects(root: &Path) -> io::Result<Vec<(String, SystemTime, u64)>> {
    let mut objects = Vec::new();
    let mut entries = match fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(objects),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        let metadata = match entry.metadata().await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_file() {
            if let Some(hash) = entry
                .file_name()
                .to_str()
                // Include interrupted atomic-write temp files in the disposable quota too.
                .filter(|name| {
                    name.len() == 64 || (name.starts_with('.') && name.ends_with(".tmp"))
                })
                .map(str::to_owned)
            {
                objects.push((
                    hash,
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len(),
                ));
            }
        }
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn concurrent_http_and_prewarm_work_share_the_same_limit() {
        let cache = ArtworkCache::new(temp_dir("permits"));
        let clone = cache.clone();
        let first = cache.processing_permit().await;
        let second = clone.processing_permit().await;
        let third = cache.processing_permit().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                clone.processing_permit()
            )
            .await
            .is_err()
        );
        let blocking_job = first.clone();
        drop(first); // A cancelled request leaves its blocking job running.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                clone.processing_permit()
            )
            .await
            .is_err()
        );
        drop(blocking_job);
        let fourth =
            tokio::time::timeout(std::time::Duration::from_secs(1), clone.processing_permit())
                .await
                .unwrap();
        drop((second, third, fourth));
    }

    #[tokio::test]
    async fn oversized_cached_image_is_not_buffered_or_served() {
        let dir = temp_dir("oversized");
        let cache = ArtworkCache::new(&dir);
        let path = cache.entry_path("abcdef", "jpg");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(33 * 1024 * 1024).unwrap();
        assert!(cache.get("abcdef", "jpg").await.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

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

    #[test]
    fn pending_retries_count_albums_and_publish_the_earliest_deadline() {
        let mut retries = PrewarmRetries::default();
        let now = std::time::Instant::now();
        assert_eq!(retries.pending(), (0, None));
        retries.failed("first", "cover", now);
        retries.failed("first", "cover", now);
        retries.failed("second", "cover", now);
        assert_eq!(
            retries.pending(),
            (2, Some(now + std::time::Duration::from_secs(300)))
        );
        retries.succeeded("second");
        assert_eq!(
            retries.pending(),
            (1, Some(now + std::time::Duration::from_secs(600)))
        );
        retries.retain(&std::collections::HashSet::new());
        assert_eq!(retries.pending(), (0, None));
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

    #[tokio::test]
    async fn identical_bytes_share_one_content_object_but_keep_source_aliases() {
        let dir = temp_dir("dedup");
        let cache = ArtworkCache::new(&dir);
        cache.put("first-key", "jpg", b"same image bytes").await;
        cache.put("second-key", "png", b"same image bytes").await;

        assert_eq!(
            cache.get("first-key", "jpg").await.as_deref(),
            Some(&b"same image bytes"[..])
        );
        assert_eq!(
            cache.get("second-key", "png").await.as_deref(),
            Some(&b"same image bytes"[..])
        );
        assert_eq!(object_count(&dir), 1);
        assert!(
            dir.join("first-key".get(..2).unwrap())
                .join("first-key.jpg")
                .exists()
        );
        assert!(
            dir.join("second-key".get(..2).unwrap())
                .join("second-key.png")
                .exists()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pins_survive_eviction_and_can_be_removed() {
        let dir = temp_dir("pins");
        let cache = ArtworkCache::new(&dir);
        cache.put("evict", "jpg", b"discard me").await;
        cache.put_pinned("chosen", "jpg", b"keep me").await.unwrap();

        let report = cache
            .maintain(0, &HashSet::from(["chosen".to_owned()]))
            .await
            .unwrap();
        assert!(report.bytes_after <= report.pinned_bytes);
        assert!(cache.get("evict", "jpg").await.is_none());
        assert_eq!(
            cache.get_pinned("chosen", "jpg").await.as_deref(),
            Some(&b"keep me"[..])
        );
        assert!(cache.delete_pinned("chosen", "jpg").await);
        assert!(cache.get_pinned("chosen", "jpg").await.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn corrupt_alias_is_a_miss_and_is_removed_without_decoding() {
        let dir = temp_dir("corrupt-alias");
        let cache = ArtworkCache::new(&dir);
        let path = cache.entry_path("broken", "jpg");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"musicata-artwork-v1\nnot-a-sha\n12\n").unwrap();

        assert!(cache.get("broken", "jpg").await.is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn source_locks_serialize_one_source_without_blocking_another() {
        let cache = ArtworkCache::new(temp_dir("source-locks"));
        let first = cache.source_lock("album").await;
        let same = cache.clone();
        let other = cache.clone();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                same.source_lock("album")
            )
            .await
            .is_err()
        );
        let other_guard = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            other.source_lock("other"),
        )
        .await
        .unwrap();
        drop((first, other_guard));
    }

    #[tokio::test]
    async fn invalidate_removes_all_source_variants_but_not_pins() {
        let dir = temp_dir("invalidate");
        let cache = ArtworkCache::new(&dir);
        cache.put("album", "jpg", b"original").await;
        cache.put("album", "300.jpg", b"thumbnail").await;
        cache.put_pinned("album", "jpg", b"manual").await.unwrap();

        assert_eq!(cache.invalidate("album").await.unwrap(), 2);
        assert!(cache.get("album", "jpg").await.is_none());
        assert!(cache.get("album", "300.jpg").await.is_none());
        assert_eq!(
            cache.get_pinned("album", "jpg").await.as_deref(),
            Some(&b"manual"[..])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn legacy_files_and_aliases_to_pins_count_toward_budget() {
        let dir = temp_dir("legacy-quota");
        let cache = ArtworkCache::new(&dir);
        let path = cache.entry_path("legacy", "jpg");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, vec![0u8; 1024 * 1024]).await.unwrap();
        cache.put_pinned("manual", "jpg", b"manual").await.unwrap();
        for n in 0..20 {
            cache.put(&format!("alias{n}"), "jpg", b"manual").await;
        }
        cache
            .maintain(0, &HashSet::from(["manual".into()]))
            .await
            .unwrap();
        assert!(!path.exists());
        for n in 0..20 {
            assert!(cache.get(&format!("alias{n}"), "jpg").await.is_none());
        }
        assert_eq!(cache.get_pinned("manual", "jpg").await.unwrap(), b"manual");
        let _ = fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn lru_order_survives_reopen_after_coalesced_maintenance() {
        let dir = temp_dir("persistent-lru");
        let cache = ArtworkCache::new(&dir);
        cache.put("old-hot", "jpg", b"older").await;
        cache.put("new-cold", "jpg", b"newer").await;
        // Explicit logical access times avoid sleeping and control ties to the second.
        cache
            .accesses
            .lock()
            .unwrap()
            .insert(sha256(b"older"), u64::MAX - 1);
        cache.maintain(u64::MAX, &HashSet::new()).await.unwrap();
        let reopened = ArtworkCache::new(&dir);
        let one = 5 + fs::metadata(cache.entry_path("old-hot", "jpg"))
            .await
            .unwrap()
            .len();
        reopened.maintain(one, &HashSet::new()).await.unwrap();
        assert_eq!(reopened.get("old-hot", "jpg").await.unwrap(), b"older");
        assert!(reopened.get("new-cold", "jpg").await.is_none());
        let _ = fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn concurrent_pin_publication_survives_zero_budget_maintenance() {
        let dir = temp_dir("pin-race");
        let cache = ArtworkCache::new(&dir);
        for n in 0..30 {
            let key = format!("pin{n}");
            let bytes = vec![n as u8; 1024];
            cache.put(&key, "jpg", &bytes).await;
            let no_pins = HashSet::new();
            let (stored, cleaned) = tokio::join!(
                cache.put_pinned(&key, "jpg", &bytes),
                cache.maintain(0, &no_pins)
            );
            stored.unwrap();
            cleaned.unwrap();
            assert_eq!(cache.get_pinned(&key, "jpg").await.unwrap(), bytes);
        }
        let _ = fs::remove_dir_all(dir).await;
    }

    fn object_count(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir.join("objects"))
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
    }
}
