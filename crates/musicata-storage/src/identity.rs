// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable source-item and selected-version identity.

use anyhow::Result;
use musicata_core::{Library, selected_version_identity, source_item_identity, source_revision};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use std::collections::{BTreeMap, BTreeSet};

use crate::{Database, TrackFingerprintTarget};

pub(crate) async fn migrate_identity_schema(pool: &SqlitePool) -> Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS library_identity_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), revision INTEGER NOT NULL)",
        "INSERT OR IGNORE INTO library_identity_state (singleton, revision) VALUES (1, 0)",
        "CREATE TABLE IF NOT EXISTS source_items (id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, provider_item_id TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)), UNIQUE(provider_id, provider_item_id))",
        "CREATE TABLE IF NOT EXISTS source_revisions (id TEXT NOT NULL, source_item_id TEXT NOT NULL, content_hash TEXT, audio_digest TEXT, audio_state TEXT NOT NULL DEFAULT 'pending' CHECK (audio_state IN ('pending','verified','unverifiable')), audio_error TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0, active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)), version_id TEXT NOT NULL, PRIMARY KEY(source_item_id,id))",
        "CREATE INDEX IF NOT EXISTS idx_source_revisions_pending ON source_revisions(active, audio_state)",
        "CREATE TABLE IF NOT EXISTS music_versions (id TEXT PRIMARY KEY, source_item_id TEXT NOT NULL, first_revision_id TEXT NOT NULL, context_kind TEXT NOT NULL DEFAULT 'unknown' CHECK (context_kind IN ('unknown','recording','release_track')), recording_id TEXT, release_track_id TEXT, snapshot_json TEXT NOT NULL, title TEXT NOT NULL, artist_name TEXT NOT NULL, album_title TEXT NOT NULL)",
    ] {
        sqlx::query(statement).execute(pool).await?;
    }
    let rows = sqlx::query("SELECT * FROM tracks").fetch_all(pool).await?;
    let mut tx = pool.begin().await?;
    for row in rows {
        let track = crate::track_from_row(&row)?;
        let id = track.id.clone();
        let provider_id = track.provider.provider_id.clone();
        let item_id = track.provider.item_id.clone();
        let slot = source_item_identity(&provider_id, &item_id);
        let revision = source_revision(&track);
        sqlx::query("INSERT OR IGNORE INTO source_items (id,provider_id,provider_item_id,active) VALUES (?1,?2,?3,1)")
            .bind(&slot).bind(&provider_id).bind(&item_id).execute(&mut *tx).await?;
        sqlx::query("INSERT OR IGNORE INTO music_versions (id,source_item_id,first_revision_id,snapshot_json,title,artist_name,album_title) VALUES (?1,?2,?3,?4,?5,?6,?7)")
            .bind(&id).bind(&slot).bind(&revision)
            .bind(serde_json::to_string(&track)?).bind(&track.title).bind(&track.artist_name).bind(&track.album_title)
            .execute(&mut *tx).await?;
        // A whole-file hash proves this exact old representation, but it cannot
        // prove a later retag. Keep it eligible for the packet worker so future
        // metadata-only revisions have retained audio evidence to compare.
        sqlx::query("INSERT OR IGNORE INTO source_revisions (id,source_item_id,content_hash,audio_state,active,version_id) VALUES (?1,?2,?3,'pending',1,?4)")
            .bind(&revision).bind(&slot).bind(&track.content_hash).bind(&id)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Call inside the snapshot-save transaction, before deleting old tracks.
pub(crate) async fn prepare_identity_save(
    tx: &mut Transaction<'_, Sqlite>,
    library: &mut Library,
) -> Result<()> {
    #[derive(Clone)]
    struct Revision {
        version_id: String,
        content_hash: Option<String>,
        active: bool,
    }

    // A full rescan commonly has thousands of unchanged entries.  Read the
    // registry once, then write only a state transition; do not turn every
    // active row off and back on just to establish that it remains present.
    let slot_rows = sqlx::query("SELECT id,active FROM source_items")
        .fetch_all(&mut **tx)
        .await?;
    let revision_rows = sqlx::query(
        "SELECT source_item_id,id,version_id,content_hash,active FROM source_revisions",
    )
    .fetch_all(&mut **tx)
    .await?;
    let version_rows = sqlx::query("SELECT id FROM music_versions")
        .fetch_all(&mut **tx)
        .await?;
    let mut slots: BTreeMap<String, bool> = slot_rows
        .into_iter()
        .map(|row| {
            Ok::<_, anyhow::Error>((row.try_get("id")?, row.try_get::<i64, _>("active")? != 0))
        })
        .collect::<Result<_>>()?;
    let mut revisions = BTreeMap::new();
    let mut by_hash = BTreeMap::new();
    for row in revision_rows {
        let slot: String = row.try_get("source_item_id")?;
        let revision: String = row.try_get("id")?;
        let entry = Revision {
            version_id: row.try_get("version_id")?,
            content_hash: row.try_get("content_hash")?,
            active: row.try_get::<i64, _>("active")? != 0,
        };
        if let Some(hash) = &entry.content_hash {
            by_hash
                .entry((slot.clone(), hash.clone()))
                .or_insert_with(|| entry.clone());
        }
        revisions.insert((slot, revision), entry);
    }
    let mut versions: BTreeSet<String> = version_rows
        .into_iter()
        .map(|row| Ok::<_, anyhow::Error>(row.try_get("id")?))
        .collect::<Result<_>>()?;
    let mut present_slots = BTreeSet::new();
    for track in &mut library.tracks {
        let slot = source_item_identity(&track.provider.provider_id, &track.provider.item_id);
        let revision = source_revision(track);
        present_slots.insert(slot.clone());
        let existing_slot = slots.contains_key(&slot);
        match slots.get(&slot).copied() {
            None => {
                sqlx::query("INSERT INTO source_items (id,provider_id,provider_item_id,active) VALUES (?1,?2,?3,1)")
                    .bind(&slot).bind(&track.provider.provider_id).bind(&track.provider.item_id).execute(&mut **tx).await?;
                slots.insert(slot.clone(), true);
            }
            Some(false) => {
                sqlx::query("UPDATE source_items SET active=1 WHERE id=?1")
                    .bind(&slot)
                    .execute(&mut **tx)
                    .await?;
                slots.insert(slot.clone(), true);
            }
            Some(true) => {}
        }
        let key = (slot.clone(), revision.clone());
        let exact_revision = revisions.get(&key).cloned();
        let exact = exact_revision.clone().or_else(|| {
            track
                .content_hash
                .as_ref()
                .and_then(|hash| by_hash.get(&(slot.clone(), hash.clone())).cloned())
        });
        let version_id: String = match exact.as_ref() {
            Some(entry) => entry.version_id.clone(),
            None if existing_slot => selected_version_identity(&slot, &revision),
            None => track.id.clone(),
        };
        track.id = version_id.clone();
        track.stream_url = format!("/api/tracks/{version_id}/stream");
        if versions.insert(version_id.clone()) {
            sqlx::query("INSERT INTO music_versions (id,source_item_id,first_revision_id,snapshot_json,title,artist_name,album_title) VALUES (?1,?2,?3,?4,?5,?6,?7)")
                .bind(&version_id).bind(&slot).bind(&revision).bind(serde_json::to_string(&*track)?).bind(&track.title).bind(&track.artist_name).bind(&track.album_title).execute(&mut **tx).await?;
        }
        // One source slot has one active representation.  A new/reappearing
        // revision first retires only that slot's previous active revision.
        // A whole-file hash can restore the selected version across a retag,
        // but it does not make this newly observed source revision registered.
        // Only the actual revision key can suppress the state transition.
        if !matches!(exact_revision.as_ref(), Some(entry) if entry.active && entry.version_id == version_id && entry.content_hash == track.content_hash)
        {
            sqlx::query(
                "UPDATE source_revisions SET active=0 WHERE source_item_id=?1 AND active=1",
            )
            .bind(&slot)
            .execute(&mut **tx)
            .await?;
            if revisions.contains_key(&key) {
                sqlx::query("UPDATE source_revisions SET active=1,version_id=?1,content_hash=?2 WHERE source_item_id=?3 AND id=?4")
                    .bind(&version_id).bind(&track.content_hash).bind(&slot).bind(&revision).execute(&mut **tx).await?;
            } else {
                sqlx::query("INSERT INTO source_revisions (id,source_item_id,content_hash,audio_state,active,version_id) VALUES (?1,?2,?3,'pending',1,?4)")
                    .bind(&revision).bind(&slot).bind(&track.content_hash).bind(&version_id).execute(&mut **tx).await?;
            }
            let entry = Revision {
                version_id: version_id.clone(),
                content_hash: track.content_hash.clone(),
                active: true,
            };
            if let Some(hash) = &entry.content_hash {
                by_hash.insert((slot.clone(), hash.clone()), entry.clone());
            }
            revisions.insert(key, entry);
        }
    }
    // A successful enumeration can remove entries, but an unchanged scan writes
    // nothing. Failed-provider retention is handled by the caller's snapshot.
    for (slot, active) in slots {
        if active && !present_slots.contains(&slot) {
            sqlx::query("UPDATE source_items SET active=0 WHERE id=?1")
                .bind(&slot)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "UPDATE source_revisions SET active=0 WHERE source_item_id=?1 AND active=1",
            )
            .bind(&slot)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

impl Database {
    pub async fn library_revision(&self) -> Result<i64> {
        Ok(
            sqlx::query("SELECT revision FROM library_identity_state WHERE singleton=1")
                .fetch_one(&self.reads)
                .await?
                .try_get("revision")?,
        )
    }

    pub async fn reconcile_library_identity(&self, library: &mut Library) -> Result<()> {
        let rows =
            sqlx::query("SELECT source_item_id,id,content_hash,version_id FROM source_revisions")
                .fetch_all(&self.reads)
                .await?;
        let slots = sqlx::query("SELECT id FROM source_items")
            .fetch_all(&self.reads)
            .await?;
        let mut by_revision = BTreeMap::new();
        let mut by_hash = BTreeMap::new();
        for row in rows {
            let slot: String = row.try_get("source_item_id")?;
            let revision: String = row.try_get("id")?;
            let version: String = row.try_get("version_id")?;
            by_revision.insert((slot.clone(), revision), version.clone());
            if let Some(hash) = row.try_get::<Option<String>, _>("content_hash")? {
                by_hash.entry((slot, hash)).or_insert(version);
            }
        }
        let slots: BTreeSet<String> = slots
            .into_iter()
            .map(|row| Ok::<_, anyhow::Error>(row.try_get("id")?))
            .collect::<Result<_>>()?;
        for track in &mut library.tracks {
            let slot = source_item_identity(&track.provider.provider_id, &track.provider.item_id);
            let revision = source_revision(track);
            if let Some(version) = by_revision.get(&(slot.clone(), revision.clone())) {
                track.id = version.clone();
                track.stream_url = format!("/api/tracks/{}/stream", track.id);
            } else if let Some(version) = track
                .content_hash
                .as_ref()
                .and_then(|hash| by_hash.get(&(slot.clone(), hash.clone())))
            {
                track.id = version.clone();
                track.stream_url = format!("/api/tracks/{}/stream", track.id);
            } else if slots.contains(&slot) {
                track.id = selected_version_identity(&slot, &revision);
                track.stream_url = format!("/api/tracks/{}/stream", track.id);
            }
        }
        Ok(())
    }

    pub async fn pending_audio_identities(
        &self,
        limit: i64,
    ) -> Result<Vec<(TrackFingerprintTarget, String)>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query("SELECT t.id,t.title,t.artist_name,t.extension,t.provider_id,t.provider_item_id,t.path,t.duration_seconds,sr.id AS revision FROM source_revisions sr JOIN tracks t ON t.id=sr.version_id WHERE sr.active=1 AND sr.next_attempt <= ?1 AND sr.audio_state IN ('pending','unverifiable') ORDER BY t.artist_name,t.title LIMIT ?2")
            .bind(crate::now_unix_seconds()).bind(limit).fetch_all(&self.reads).await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    TrackFingerprintTarget {
                        track_id: row.try_get("id")?,
                        title: row.try_get("title")?,
                        artist_name: row.try_get("artist_name")?,
                        extension: row.try_get("extension")?,
                        provider_id: row.try_get("provider_id")?,
                        provider_item_id: row.try_get("provider_item_id")?,
                        path: row.try_get("path")?,
                        duration_seconds: row.try_get("duration_seconds")?,
                    },
                    row.try_get("revision")?,
                ))
            })
            .collect()
    }

    pub async fn record_audio_identity(
        &self,
        track_id: &str,
        revision: &str,
        result: std::result::Result<&str, &str>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Reserve SQLite's single writer before reads; a worker must retry
        // rather than failing a deferred read-to-write upgrade.
        sqlx::query("UPDATE library_identity_state SET revision=revision WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        let current = sqlx::query("SELECT source_item_id FROM source_revisions WHERE id=?1 AND version_id=?2 AND active=1")
            .bind(revision).bind(track_id).fetch_optional(&mut *tx).await?;
        let Some(current) = current else {
            tx.rollback().await?;
            return Ok(false);
        };
        let actual = sqlx::query("SELECT * FROM tracks WHERE id=?1")
            .bind(track_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(actual) = actual else {
            tx.rollback().await?;
            return Ok(false);
        };
        if source_revision(&crate::track_from_row(&actual)?) != revision {
            tx.rollback().await?;
            return Ok(false);
        }
        let slot: String = current.try_get("source_item_id")?;
        if let Err(error) = result {
            let long = error.contains("requires") || error.contains("cannot prove");
            let now = crate::now_unix_seconds();
            let changed = sqlx::query("UPDATE source_revisions SET audio_state=CASE WHEN ?1 THEN 'unverifiable' ELSE 'pending' END,audio_error=?2,next_attempt=?3 + CASE WHEN ?1 THEN 86400 ELSE min(21600,300*(1 << min(attempts,6))) END,attempts=attempts+1 WHERE id=?4 AND version_id=?5 AND active=1")
                .bind(long).bind(error).bind(now).bind(revision).bind(track_id).execute(&mut *tx).await?.rows_affected() == 1;
            tx.commit().await?;
            return Ok(changed);
        }
        let digest = result.expect("checked above");
        // One source slot may accumulate several metadata-only revisions of the
        // same exact payload.  Preserve the first selected version as their
        // canonical identity; `active` alone is unstable and would make the
        // result depend on worker timing.
        let previous = sqlx::query("SELECT version_id FROM source_revisions WHERE source_item_id=?1 AND audio_digest=?2 AND id<>?3 ORDER BY rowid ASC LIMIT 1")
            .bind(&slot).bind(digest).bind(revision).fetch_optional(&mut *tx).await?;
        sqlx::query("UPDATE source_revisions SET audio_state='verified',audio_digest=?1,audio_error=NULL,attempts=attempts+1 WHERE id=?2 AND version_id=?3 AND active=1")
            .bind(digest).bind(revision).bind(track_id).execute(&mut *tx).await?;
        if let Some(previous) = previous {
            let old: String = previous.try_get("version_id")?;
            if old != track_id {
                remap_temporary_version(&mut tx, track_id, &old).await?;
                sqlx::query(
                    "UPDATE source_revisions SET version_id=?1 WHERE source_item_id=?2 AND id=?3",
                )
                .bind(&old)
                .bind(&slot)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
                // Keep every proved representation of this payload pointed at
                // the same selected version, so a later retag cannot choose a
                // different historical row based on query order.
                sqlx::query(
                    "UPDATE source_revisions SET version_id=?1 WHERE source_item_id=?2 AND audio_digest=?3",
                )
                .bind(&old)
                .bind(&slot)
                .bind(digest)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE library_identity_state SET revision=revision+1 WHERE singleton=1",
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }
}

async fn remap_temporary_version(
    tx: &mut Transaction<'_, Sqlite>,
    temporary: &str,
    old: &str,
) -> Result<()> {
    // Metadata rows have immediate foreign keys. Defer validation while this
    // transaction moves dependants before restoring the old parent key.
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE OR IGNORE favorites SET item_id=?1 WHERE item_type='track' AND item_id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM favorites WHERE item_type='track' AND item_id=?1")
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    // A queued scrobble describes the same now-proven representation; completed
    // listens are deliberately immutable and therefore excluded.
    sqlx::query("UPDATE scrobble_queue SET track_id=?1 WHERE track_id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    remap_grouping_origins(tx, temporary, old).await?;
    for table in ["playlist_tracks", "player_queue_items", "zone_queue_items"] {
        let sql = if table == "playlist_tracks" {
            format!("UPDATE {table} SET track_id=?1 WHERE track_id=?2")
        } else {
            format!(
                "UPDATE {table} SET track_id=?1,stream_url=REPLACE(stream_url, '/api/tracks/' || ?2 || '/stream', '/api/tracks/' || ?1 || '/stream') WHERE track_id=?2"
            )
        };
        sqlx::query(&sql)
            .bind(old)
            .bind(temporary)
            .execute(&mut **tx)
            .await?;
    }
    for table in [
        "track_metadata_observations",
        "track_metadata_field_observations",
    ] {
        sqlx::query(&format!("UPDATE {table} SET track_id=?1 WHERE track_id=?2"))
            .bind(old)
            .bind(temporary)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("UPDATE tracks SET id=?1,stream_url='/api/tracks/' || ?1 || '/stream' WHERE id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    // The old selected version is the canonical key after an exact-payload
    // proof. Preserve its existing source analyses; transient work started for
    // the temporary id is discarded and will be scheduled against the old key.
    for table in [
        "track_fingerprint",
        "track_loudness",
        "track_features",
        "track_embedding",
        "track_musicbrainz_metadata",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE track_id=?1"))
            .bind(temporary)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn remap_grouping_origins(
    tx: &mut Transaction<'_, Sqlite>,
    temporary: &str,
    old: &str,
) -> Result<()> {
    for table in [
        "grouping_favorite_origins",
        "grouping_album_artwork_origins",
        "grouping_artist_artwork_origins",
        "grouping_artwork_attempt_origins",
        "grouping_artwork_refresh_origins",
    ] {
        sqlx::query(&format!(
            "UPDATE OR IGNORE {table} SET track_id=?1 WHERE track_id=?2"
        ))
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!("DELETE FROM {table} WHERE track_id=?1"))
            .bind(temporary)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("UPDATE OR IGNORE grouping_favorite_origins SET selection_id=?1 WHERE selection_type='track' AND selection_id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "DELETE FROM grouping_favorite_origins WHERE selection_type='track' AND selection_id=?1",
    )
    .bind(temporary)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE OR IGNORE grouping_favorite_projections SET selection_id=?1 WHERE selection_type='track' AND selection_id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM grouping_favorite_projections WHERE selection_type='track' AND selection_id=?1")
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE OR IGNORE grouping_favorite_projections SET item_id=?1 WHERE item_type='track' AND item_id=?2")
        .bind(old)
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM grouping_favorite_projections WHERE item_type='track' AND item_id=?1")
        .bind(temporary)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ListenKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    async fn fixture() -> (Database, Library, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "musicata-identity-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Album")).unwrap();
        std::fs::write(root.join("Album/Artist - Song.mp3"), b"first payload").unwrap();
        let library = musicata_core::scan_local_library(&root).unwrap();
        let db = Database::connect(root.join("library.db")).await.unwrap();
        (db, library, root)
    }

    #[tokio::test]
    async fn digest_equivalence_restores_the_existing_selected_version() {
        let (db, mut library, _) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        let old_revision = source_revision(&library.tracks[0]);
        assert!(
            db.record_audio_identity(&old, &old_revision, Ok("packets-v1:original"))
                .await
                .unwrap()
        );

        library.tracks[0].content_hash = Some("retagged-container".into());
        library.tracks[0].title = "Corrected title".into();
        db.save_library(&mut library).await.unwrap();
        let temporary = library.tracks[0].id.clone();
        assert_ne!(temporary, old);
        let new_revision = source_revision(&library.tracks[0]);
        assert!(
            !db.record_audio_identity(&temporary, &old_revision, Ok("packets-v1:original"))
                .await
                .unwrap()
        );
        assert!(
            db.record_audio_identity(&temporary, &new_revision, Ok("packets-v1:original"))
                .await
                .unwrap()
        );
        assert_eq!(db.load_library().await.unwrap().unwrap().tracks[0].id, old);
        assert_eq!(
            db.track(&temporary).await.unwrap().unwrap().id,
            old,
            "a live queue holding the provisional id must resolve only its verified version"
        );
        let versions: Vec<String> =
            sqlx::query("SELECT version_id FROM source_revisions ORDER BY rowid")
                .fetch_all(&db.pool)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get("version_id").unwrap())
                .collect();
        assert_eq!(versions, vec![old.clone(), old]);
    }

    #[tokio::test]
    async fn unavailable_old_version_is_not_rewritten_to_a_replacement() {
        let (db, mut library, _) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        db.record_listen(&old, "browser", 123, ListenKind::Played)
            .await
            .unwrap();
        library.tracks[0].content_hash = Some("different-recording".into());
        library.tracks[0].title = "Different song".into();
        db.save_library(&mut library).await.unwrap();
        assert_ne!(library.tracks[0].id, old);
        let history = db.recently_played(10).await.unwrap();
        assert_eq!(history[0].0.id, old);
    }

    #[tokio::test]
    async fn absence_and_reappearance_restore_selected_version() {
        let (db, mut library, _) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        let mut empty = library.clone();
        empty.tracks.clear();
        empty.albums.clear();
        empty.artists.clear();
        db.save_library(&mut empty).await.unwrap();
        library.tracks[0].id = "fresh-scan-candidate".into();
        db.save_library(&mut library).await.unwrap();
        assert_eq!(library.tracks[0].id, old);
    }

    #[tokio::test]
    async fn stale_snapshot_cannot_overwrite_a_newer_save() {
        let (db, mut library, _) = fixture().await;
        let revision = db.library_revision().await.unwrap();
        db.save_library(&mut library).await.unwrap();
        let mut stale = library.clone();
        stale.tracks.clear();
        assert!(
            db.save_library_at_revision(&mut stale, revision)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn exact_equivalence_remaps_favorites_playlists_queues_and_origins() {
        let (db, mut library, _) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        let old_revision = source_revision(&library.tracks[0]);
        db.record_audio_identity(&old, &old_revision, Ok("packet"))
            .await
            .unwrap();
        library.tracks[0].content_hash = Some("retagged-container".into());
        db.save_library(&mut library).await.unwrap();
        let temporary = library.tracks[0].id.clone();
        let new_revision = source_revision(&library.tracks[0]);
        sqlx::query("INSERT INTO favorites (item_type,item_id,created_at_unix_seconds) VALUES ('track',?1,1)")
            .bind(&temporary)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO playlists (id,name,comment,created_at_unix_seconds,updated_at_unix_seconds) VALUES ('p','P',NULL,1,1)")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO playlist_tracks (playlist_id,position,track_id) VALUES ('p',0,?1)",
        )
        .bind(&temporary)
        .execute(&db.pool)
        .await
        .unwrap();
        for table in ["player_queue_items", "zone_queue_items"] {
            let owner = if table.starts_with("player") {
                "player_id"
            } else {
                "zone_id"
            };
            let owner_value = if table.starts_with("player") {
                "x"
            } else {
                "z"
            };
            sqlx::query(&format!("INSERT INTO {table} ({owner},seq,track_id,title,artist,album,stream_url,artwork_url) VALUES (?1,0,?2,'t','a','b','/api/tracks/' || ?2 || '/stream',NULL)"))
                .bind(owner_value).bind(&temporary).execute(&db.pool).await.unwrap();
        }
        sqlx::query("INSERT INTO grouping_favorite_origins (selection_type,selection_id,track_id,role,created_at_unix_seconds) VALUES ('track',?1,?1,'track',1)")
            .bind(&temporary).execute(&db.pool).await.unwrap();
        db.record_audio_identity(&temporary, &new_revision, Ok("packet"))
            .await
            .unwrap();
        for sql in [
            "SELECT item_id FROM favorites WHERE item_type='track'",
            "SELECT track_id FROM playlist_tracks WHERE playlist_id='p'",
            "SELECT track_id FROM player_queue_items WHERE player_id='x'",
            "SELECT track_id FROM zone_queue_items WHERE zone_id='z'",
            "SELECT track_id FROM grouping_favorite_origins WHERE selection_type='track'",
        ] {
            let value: String = sqlx::query(sql)
                .fetch_one(&db.pool)
                .await
                .unwrap()
                .try_get(0)
                .unwrap();
            assert_eq!(value, old);
        }
    }

    #[tokio::test]
    async fn large_mtime_change_restores_only_after_matching_packet_digest() {
        let (db, mut library, _) = fixture().await;
        library.tracks[0].content_hash = None;
        library.tracks[0].modified_at_unix_seconds = Some(10);
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        let old_revision = source_revision(&library.tracks[0]);
        db.record_audio_identity(&old, &old_revision, Ok("packet"))
            .await
            .unwrap();
        library.tracks[0].modified_at_unix_seconds = Some(11);
        db.save_library(&mut library).await.unwrap();
        let temporary = library.tracks[0].id.clone();
        assert_ne!(temporary, old);
        let revision = source_revision(&library.tracks[0]);
        db.record_audio_identity(&temporary, &revision, Ok("packet"))
            .await
            .unwrap();
        assert_eq!(db.load_library().await.unwrap().unwrap().tracks[0].id, old);
    }

    #[tokio::test]
    async fn equal_full_file_hash_preserves_version_without_packet_guessing() {
        let (db, mut library, _) = fixture().await;
        library.tracks[0].content_hash = Some("exact-whole-file".into());
        library.tracks[0].modified_at_unix_seconds = Some(10);
        db.save_library(&mut library).await.unwrap();
        let old = library.tracks[0].id.clone();
        library.tracks[0].modified_at_unix_seconds = Some(11);
        db.save_library(&mut library).await.unwrap();
        assert_eq!(library.tracks[0].id, old);
    }

    #[tokio::test]
    async fn full_file_hash_retag_registers_the_new_revision_for_audio_work() {
        let (db, mut library, _) = fixture().await;
        library.tracks[0].content_hash = Some("exact-whole-file".into());
        library.tracks[0].modified_at_unix_seconds = Some(10);
        db.save_library(&mut library).await.unwrap();
        let selected = library.tracks[0].id.clone();
        let old_revision = source_revision(&library.tracks[0]);

        library.tracks[0].modified_at_unix_seconds = Some(11);
        db.save_library(&mut library).await.unwrap();
        let revision = source_revision(&library.tracks[0]);
        assert_ne!(revision, old_revision);
        assert_eq!(library.tracks[0].id, selected);

        let pending = db.pending_audio_identities(10).await.unwrap();
        assert!(
            pending
                .iter()
                .any(|(target, token)| target.track_id == selected && token == &revision)
        );
    }

    #[tokio::test]
    async fn reconciliation_is_read_only_for_an_unchanged_scan() {
        let (db, mut library, _) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let before: i64 = sqlx::query("SELECT total_changes()")
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        db.reconcile_library_identity(&mut library).await.unwrap();
        let after: i64 = sqlx::query("SELECT total_changes()")
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn unchanged_eleven_thousand_item_prepare_writes_no_registry_rows() {
        let (db, mut library, _) = fixture().await;
        let template = library.tracks[0].clone();
        library.tracks = (0..11_000)
            .map(|index| {
                let mut track = template.clone();
                track.id = format!("bulk-{index}");
                track.provider.item_id = format!("bulk/{index}.mp3");
                track.relative_path = track.provider.item_id.clone();
                track.stream_url = format!("/api/tracks/bulk-{index}/stream");
                track
            })
            .collect();
        db.save_library(&mut library).await.unwrap();
        for (table, action) in [
            ("source_items", "INSERT"),
            ("source_items", "UPDATE"),
            ("source_revisions", "INSERT"),
            ("source_revisions", "UPDATE"),
            ("music_versions", "INSERT"),
        ] {
            sqlx::query(&format!(
                "CREATE TRIGGER deny_{table}_{action}
                 BEFORE {action} ON {table}
                 BEGIN SELECT RAISE(FAIL, 'unexpected unchanged registry write'); END"
            ))
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let mut tx = db.pool.begin().await.unwrap();
        prepare_identity_save(&mut tx, &mut library).await.unwrap();
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn v37_migration_preserves_ids_references_snapshots_and_integrity() {
        let (db, mut library, root) = fixture().await;
        db.save_library(&mut library).await.unwrap();
        let track = library.tracks[0].id.clone();
        db.set_favorite("track", &track, 1).await.unwrap();
        let playlist = db
            .create_playlist("P", None, std::slice::from_ref(&track), 1)
            .await
            .unwrap();
        db.set_playlist_tracks(&playlist, std::slice::from_ref(&track), 2)
            .await
            .unwrap();
        db.record_listen(&track, "browser", 3, ListenKind::Played)
            .await
            .unwrap();
        let snapshot = db.recently_played(1).await.unwrap()[0].0.clone();
        for table in [
            "source_revisions",
            "source_items",
            "music_versions",
            "library_identity_state",
            "grouping_favorite_projections",
            "grouping_favorite_origins",
            "grouping_album_artwork_origins",
            "grouping_artist_artwork_origins",
            "grouping_artwork_attempt_origins",
            "grouping_artwork_refresh_origins",
        ] {
            sqlx::query(&format!("DROP TABLE {table}"))
                .execute(&db.pool)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 37")
            .execute(&db.pool)
            .await
            .unwrap();
        drop(db);
        let reopened = Database::connect(root.join("library.db")).await.unwrap();
        assert_eq!(
            reopened.load_library().await.unwrap().unwrap().tracks[0].id,
            track
        );
        assert_eq!(
            reopened.favorite_ids("track").await.unwrap(),
            vec![track.clone()]
        );
        assert_eq!(
            reopened.playlist_track_ids(&playlist).await.unwrap(),
            vec![track]
        );
        let restored_snapshot = reopened.recently_played(1).await.unwrap()[0].0.clone();
        assert_eq!(restored_snapshot.id, snapshot.id);
        assert_eq!(restored_snapshot.title, snapshot.title);
        assert_eq!(restored_snapshot.album_title, snapshot.album_title);
        let integrity: String = sqlx::query("PRAGMA quick_check")
            .fetch_one(&reopened.pool)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}
