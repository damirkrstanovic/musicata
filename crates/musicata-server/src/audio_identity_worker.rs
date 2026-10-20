// SPDX-License-Identifier: AGPL-3.0-or-later
//! Background persistence for exact encoded-audio identity evidence.
//!
//! This worker deliberately owns no scan or grouping state. Storage hands it a source-revision
//! compare-and-swap token; a result produced after a rescan is therefore harmless.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use symphonia::core::io::MediaSource;
use tokio::sync::RwLock;

use crate::providers::ProviderRegistry;

const BATCH: i64 = 8;
/// The core scanner stores a full container hash only at or below this size.
const INLINE_CONTENT_HASH_LIMIT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceState {
    bytes: u64,
    modified: SystemTime,
    modified_at_unix_seconds: i64,
}

fn source_state(bytes: u64, modified: SystemTime) -> Result<SourceState, String> {
    let modified_at_unix_seconds = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("source modification time predates Unix epoch: {error}"))?
        .as_secs() as i64;
    Ok(SourceState {
        bytes,
        modified,
        modified_at_unix_seconds,
    })
}

async fn read_source_state(
    providers: &Arc<RwLock<ProviderRegistry>>,
    target: &musicata_storage::TrackFingerprintTarget,
) -> Result<SourceState, String> {
    #[cfg(feature = "provider-smb")]
    if let Some(smb) = {
        let providers = providers.read().await;
        match providers.get(&target.provider_id) {
            Some(crate::providers::ProviderHandle::Smb(smb)) => Some(smb.clone()),
            _ => None,
        }
    } {
        let (bytes, modified) = smb
            .audio_stat(&target.provider_item_id)
            .await
            .map_err(|error| format!("SMB source metadata: {error}"))?;
        return source_state(bytes, modified);
    }
    #[cfg(not(feature = "provider-smb"))]
    let _ = providers;
    let metadata = tokio::fs::metadata(&target.path)
        .await
        .map_err(|error| format!("source metadata: {error}"))?;
    source_state(
        metadata.len(),
        metadata
            .modified()
            .map_err(|error| format!("source modification time: {error}"))?,
    )
}

/// Prevent a preserved size/mtime from attaching current packet evidence to an older scanned
/// revision. Large files have no scanner content hash, so their stat guard remains the available
/// evidence and cannot detect an adversarial same-size, same-mtime replacement; small files must
/// match the scanner's complete container digest first.
fn digest_verified_source(
    mut source: Box<dyn MediaSource>,
    extension: &str,
    expected_hash: Option<&str>,
    expected_bytes: u64,
) -> Result<String, String> {
    if let Some(expected_hash) = expected_hash {
        if expected_bytes > INLINE_CONTENT_HASH_LIMIT_BYTES {
            return Err("stored content hash exceeds the scanner small-file limit".to_string());
        }
        let mut hasher = Sha256::new();
        let mut remaining = expected_bytes;
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            let request = usize::try_from(remaining)
                .unwrap_or(buffer.len())
                .min(buffer.len());
            let read = source
                .read(&mut buffer[..request])
                .map_err(|error| format!("source read while checking content hash: {error}"))?;
            if read == 0 {
                return Err("source ended before its scanned content hash length".to_string());
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        let mut extra = [0_u8; 1];
        if source
            .read(&mut extra)
            .map_err(|error| format!("source read while checking content hash: {error}"))?
            != 0
        {
            return Err("source exceeded its scanned content hash length".to_string());
        }
        if format!("{:x}", hasher.finalize()) != expected_hash {
            return Err("source bytes no longer match the scanned content hash".to_string());
        }
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("source rewind after content hash check: {error}"))?;
    }
    crate::audio_identity::digest_source(source, extension)
}

/// Process one bounded batch. `Ok(0)` means storage had no work; callers can then idle.
pub async fn audio_identity_pass(
    database: &musicata_storage::Database,
    providers: &Arc<RwLock<ProviderRegistry>>,
) -> Result<usize, String> {
    let pending = database
        .pending_audio_identities(BATCH)
        .await
        .map_err(|error| format!("audio identity queue: {error}"))?;
    let mut recorded = 0;

    // Digest one source at a time. This is intentionally conservative: each item streams the
    // complete encoded payload and can hold an SMB connection for minutes. The outer loop drains
    // immediately while work remains, so this is a concurrency bound rather than a polling delay.
    for (target, revision) in pending {
        let mut job = crate::diagnostics::runtime::Job::new("audio_identity");
        let track_id = target.track_id.clone();
        // We need stable size + mtime both before and after hashing. Providers without
        // comparable source attributes are deliberately unverifiable.
        let before = read_source_state(providers, &target).await;
        let result = match before {
            Err(error) => Err(format!("source is unverifiable: {error}")),
            Ok(before) => {
                let extension = target.extension.clone();
                let current = database
                    .track(&track_id)
                    .await
                    .map_err(|error| format!("audio identity track lookup: {error}"))?;
                if current.as_ref().is_none_or(|track| {
                    musicata_core::source_revision(track) != revision
                        || track.file_size_bytes != Some(before.bytes)
                        || track.modified_at_unix_seconds != Some(before.modified_at_unix_seconds)
                }) {
                    Err("source changed before audio identity digest".to_string())
                } else {
                    let expected_hash = current.and_then(|track| track.content_hash);
                    match crate::open_track_analysis_source(providers, &target).await {
                        Ok(source) => match tokio::task::spawn_blocking(move || {
                            digest_verified_source(
                                source,
                                &extension,
                                expected_hash.as_deref(),
                                before.bytes,
                            )
                        })
                        .await
                        .unwrap_or_else(|_| Err("audio identity task panicked".to_string()))
                        {
                            Ok(digest) => match read_source_state(providers, &target).await {
                                Ok(after) if after == before => Ok(digest),
                                Ok(_) => {
                                    Err("source changed during audio identity digest".to_string())
                                }
                                Err(error) => {
                                    Err(format!("source is unverifiable after digest: {error}"))
                                }
                            },
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(format!("source read: {error}")),
                    }
                }
            }
        };
        let result_ref = result.as_ref().map(String::as_str).map_err(String::as_str);
        let persisted = database
            .record_audio_identity(&track_id, &revision, result_ref)
            .await
            .map_err(|error| format!("audio identity persistence: {error}"))?;
        if persisted {
            recorded += 1;
            if result.is_ok() {
                job.success();
            } else {
                job.fail(true, "source_unverifiable");
            }
        } else {
            job.fail(true, "stale_revision");
        }
    }
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8], output: &mut Vec<u8>) {
        output.extend_from_slice(id);
        output.extend_from_slice(&(body.len() as u32).to_le_bytes());
        output.extend_from_slice(body);
        if body.len() % 2 != 0 {
            output.push(0);
        }
    }

    fn wav(samples: &[u8], title: Option<&[u8]>) -> Vec<u8> {
        let mut chunks = Vec::new();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&8u16.to_le_bytes());
        chunk(b"fmt ", &fmt, &mut chunks);
        if let Some(title) = title {
            let mut list = b"INFO".to_vec();
            chunk(b"INAM", title, &mut list);
            chunk(b"LIST", &list, &mut chunks);
        }
        chunk(b"data", samples, &mut chunks);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(4 + chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&chunks);
        out
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "musicata-audio-identity-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn local_source_state_rejects_a_changed_file() {
        let path = std::env::temp_dir().join(format!(
            "musicata-audio-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"first").unwrap();
        let before = tokio::fs::metadata(&path).await.unwrap();
        std::fs::write(&path, b"second payload").unwrap();
        let after = tokio::fs::metadata(&path).await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert_ne!(before.len(), after.len());
    }

    #[tokio::test]
    async fn stale_source_is_rejected_before_worker_hashes_it() {
        let root = std::env::temp_dir().join(format!(
            "musicata-audio-identity-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Album")).unwrap();
        let audio = root.join("Album/Artist - Song.wav");
        std::fs::write(&audio, wav(&[128; 32], None)).unwrap();
        let mut library = musicata_core::scan_local_library(&root).unwrap();
        let database = musicata_storage::Database::connect(root.join("library.db"))
            .await
            .unwrap();
        database.save_library(&mut library).await.unwrap();

        std::fs::write(&audio, wav(&[128; 64], None)).unwrap();
        let providers = Arc::new(RwLock::new(ProviderRegistry::new()));
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn wav_retag_restores_selected_version_and_changed_audio_remains_unplayable() {
        let root = temp_root("worker-e2e");
        std::fs::create_dir_all(root.join("Album")).unwrap();
        let audio = root.join("Album/Artist - Song.wav");
        let samples = [128, 129, 130, 131];
        std::fs::write(&audio, wav(&samples, Some(b"Original"))).unwrap();
        let database_path = root.join("library.db");
        let database = musicata_storage::Database::connect(&database_path)
            .await
            .unwrap();
        let mut initial = musicata_core::scan_local_library(&root).unwrap();
        database.save_library(&mut initial).await.unwrap();
        let old_id = initial.tracks[0].id.clone();
        let playlist = database
            .create_playlist("Saved", None, std::slice::from_ref(&old_id), 1)
            .await
            .unwrap();
        database.set_favorite("track", &old_id, 2).await.unwrap();
        let providers = Arc::new(RwLock::new(ProviderRegistry::new()));
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);

        // A LIST/INFO title update changes container bytes and source revision but not packets.
        std::fs::write(&audio, wav(&samples, Some(b"Retagged title"))).unwrap();
        let prior = database.load_library().await.unwrap().unwrap();
        let mut retagged =
            musicata_core::scan_local_library_incremental(&root, Some(&prior), &mut |_| {})
                .unwrap();
        database
            .reconcile_library_identity(&mut retagged)
            .await
            .unwrap();
        assert_ne!(retagged.tracks[0].id, old_id);
        database.save_library(&mut retagged).await.unwrap();
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);
        assert!(database.track(&old_id).await.unwrap().is_some());
        assert_eq!(
            database.playlist_tracks(&playlist).await.unwrap()[0].id,
            old_id
        );
        assert_eq!(database.starred_tracks().await.unwrap()[0].id, old_id);

        // Selection restoration survives reopening the database.
        drop(database);
        let database = musicata_storage::Database::connect(&database_path)
            .await
            .unwrap();
        assert!(database.track(&old_id).await.unwrap().is_some());

        // A PCM change is a different payload. The old selection remains displayable but cannot
        // be streamed because it is no longer an active track.
        std::fs::write(&audio, wav(&[128, 129, 130, 132], Some(b"Retagged title"))).unwrap();
        let prior = database.load_library().await.unwrap().unwrap();
        let mut changed =
            musicata_core::scan_local_library_incremental(&root, Some(&prior), &mut |_| {})
                .unwrap();
        database
            .reconcile_library_identity(&mut changed)
            .await
            .unwrap();
        database.save_library(&mut changed).await.unwrap();
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);
        assert_ne!(changed.tracks[0].id, old_id);
        assert!(database.track(&old_id).await.unwrap().is_none());
        let selected = database.playlist_tracks(&playlist).await.unwrap();
        assert_eq!(selected[0].id, old_id);
        assert_eq!(selected[0].title, initial.tracks[0].title);
        assert_eq!(database.starred_tracks().await.unwrap()[0].id, old_id);

        drop(database);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn missing_source_is_backed_off_after_worker_failure() {
        let root = temp_root("missing-source");
        std::fs::create_dir_all(root.join("Album")).unwrap();
        let audio = root.join("Album/Artist - Song.wav");
        std::fs::write(&audio, wav(&[128; 32], None)).unwrap();
        let mut library = musicata_core::scan_local_library(&root).unwrap();
        let database = musicata_storage::Database::connect(root.join("library.db"))
            .await
            .unwrap();
        database.save_library(&mut library).await.unwrap();
        std::fs::remove_file(&audio).unwrap();

        let providers = Arc::new(RwLock::new(ProviderRegistry::new()));
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);
        assert!(
            database
                .pending_audio_identities(8)
                .await
                .unwrap()
                .is_empty()
        );

        drop(database);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn preserved_stat_small_file_change_cannot_verify_the_old_revision() {
        let root = temp_root("preserved-stat-content-hash");
        std::fs::create_dir_all(root.join("Album")).unwrap();
        let audio = root.join("Album/Artist - Song.wav");
        std::fs::write(&audio, wav(&[128, 129, 130, 131], None)).unwrap();
        let database_path = root.join("library.db");
        let database = musicata_storage::Database::connect(&database_path)
            .await
            .unwrap();
        let mut initial = musicata_core::scan_local_library(&root).unwrap();
        database.save_library(&mut initial).await.unwrap();
        let old_id = initial.tracks[0].id.clone();
        let original_modified = std::fs::metadata(&audio).unwrap().modified().unwrap();

        // Same container length and restored mtime defeat the stat guard alone. The scanner's
        // small-file hash is the remaining evidence that this is no longer the old revision.
        std::fs::write(&audio, wav(&[128, 129, 130, 132], None)).unwrap();
        std::fs::File::open(&audio)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        let providers = Arc::new(RwLock::new(ProviderRegistry::new()));
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);
        assert!(
            database
                .pending_audio_identities(8)
                .await
                .unwrap()
                .is_empty()
        );

        // Once the scanner sees the changed bytes, their digest must create a distinct current
        // version rather than equating them with the historical selection.
        let prior = database.load_library().await.unwrap().unwrap();
        let mut changed =
            musicata_core::scan_local_library_incremental(&root, Some(&prior), &mut |_| {})
                .unwrap();
        database
            .reconcile_library_identity(&mut changed)
            .await
            .unwrap();
        database.save_library(&mut changed).await.unwrap();
        assert_eq!(audio_identity_pass(&database, &providers).await.unwrap(), 1);
        assert!(database.track(&old_id).await.unwrap().is_none());

        drop(database);
        std::fs::remove_dir_all(root).unwrap();
    }
}
