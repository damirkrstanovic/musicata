// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reversible projections for artist and album grouping.
//!
//! A favorite belongs to the source-track memberships selected at the time the user
//! starred a card.  Derived artist and album ids are only the current projection of
//! those memberships, so an alias merge can later be removed without losing intent.

use anyhow::Result;
use musicata_core::Library;
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Role {
    Track,
    Album,
    TrackArtist,
    AlbumArtist,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::TrackArtist => "track_artist",
            Self::AlbumArtist => "album_artist",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Membership {
    item_type: &'static str,
    item_id: String,
    track_id: String,
    role: Role,
}

/// Schema is deliberately provenance-only: source rows are never renamed from a
/// predicted display id. The save hook calls the projection before replacing rows.
pub(crate) async fn migrate_grouping_provenance(pool: &SqlitePool) -> Result<()> {
    for sql in [
        "CREATE TABLE IF NOT EXISTS grouping_favorite_origins (selection_type TEXT NOT NULL, selection_id TEXT NOT NULL, track_id TEXT NOT NULL, role TEXT NOT NULL, created_at_unix_seconds INTEGER NOT NULL, PRIMARY KEY(selection_type, selection_id, track_id, role))",
        "CREATE TABLE IF NOT EXISTS grouping_favorite_projections (selection_type TEXT NOT NULL, selection_id TEXT NOT NULL, item_type TEXT NOT NULL, item_id TEXT NOT NULL, PRIMARY KEY(selection_type, selection_id, item_type, item_id))",
        "CREATE TABLE IF NOT EXISTS grouping_album_artwork_origins (source_album_id TEXT NOT NULL, track_id TEXT NOT NULL, cache_key TEXT, ext TEXT, candidate_id TEXT, provider TEXT, remote_url TEXT, width INTEGER, status TEXT, acquired_at_unix_seconds INTEGER, role TEXT NOT NULL DEFAULT 'album', PRIMARY KEY(source_album_id, track_id, role))",
        "CREATE TABLE IF NOT EXISTS grouping_artwork_attempt_origins (source_album_id TEXT NOT NULL, track_id TEXT NOT NULL, revision TEXT NOT NULL, absent INTEGER NOT NULL, failures INTEGER NOT NULL, next_attempt INTEGER NOT NULL, role TEXT NOT NULL DEFAULT 'album', PRIMARY KEY(source_album_id, track_id, role))",
        "CREATE TABLE IF NOT EXISTS grouping_artist_artwork_origins (source_artist_id TEXT NOT NULL, track_id TEXT NOT NULL, role TEXT NOT NULL, snapshot_json TEXT NOT NULL, PRIMARY KEY(source_artist_id,track_id,role))",
        "CREATE TABLE IF NOT EXISTS grouping_artwork_refresh_origins (source_album_id TEXT NOT NULL, track_id TEXT NOT NULL, generation INTEGER NOT NULL, role TEXT NOT NULL DEFAULT 'album', PRIMARY KEY(source_album_id, track_id, role))",
    ] {
        sqlx::query(sql).execute(pool).await?;
    }
    Ok(())
}

fn memberships(library: &Library) -> Vec<Membership> {
    let album_artists: BTreeMap<_, _> = library
        .albums
        .iter()
        .map(|album| (album.id.as_str(), album.artist_id.as_str()))
        .collect();
    let mut result = Vec::with_capacity(library.tracks.len() * 4);
    for track in &library.tracks {
        result.push(Membership {
            item_type: "track",
            item_id: track.id.clone(),
            track_id: track.id.clone(),
            role: Role::Track,
        });
        result.push(Membership {
            item_type: "album",
            item_id: track.album_id.clone(),
            track_id: track.id.clone(),
            role: Role::Album,
        });
        result.push(Membership {
            item_type: "artist",
            item_id: track.artist_id.clone(),
            track_id: track.id.clone(),
            role: Role::TrackArtist,
        });
        if let Some(artist_id) = album_artists.get(track.album_id.as_str()) {
            result.push(Membership {
                item_type: "artist",
                item_id: (*artist_id).to_string(),
                track_id: track.id.clone(),
                role: Role::AlbumArtist,
            });
        }
    }
    result.sort();
    result.dedup();
    result
}

/// Capture a new favorite's current members. Call this in the same transaction as
/// the normal `favorites` insert. The migration creates `grouping_favorite_origins`.
pub(crate) async fn capture_favorite_origins(
    tx: &mut Transaction<'_, Sqlite>,
    library: &Library,
    item_type: &str,
    item_id: &str,
    created_at: i64,
) -> Result<()> {
    for member in memberships(library)
        .into_iter()
        .filter(|member| member.item_type == item_type && member.item_id == item_id)
    {
        sqlx::query(
            "INSERT INTO grouping_favorite_origins
                (selection_type, selection_id, track_id, role, created_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(selection_type, selection_id, track_id, role) DO NOTHING",
        )
        .bind(item_type)
        .bind(item_id)
        .bind(member.track_id)
        .bind(member.role.as_str())
        .bind(created_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Seed origins for favorites written before provenance tracking existed. This is
/// deliberately based on actual before-library membership, never names or guessed ids.
pub(crate) async fn seed_legacy_favorite_origins(
    tx: &mut Transaction<'_, Sqlite>,
    before: &Library,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT item_type, item_id, created_at_unix_seconds FROM favorites
         WHERE item_type IN ('track', 'album', 'artist')
         AND NOT EXISTS (SELECT 1 FROM grouping_favorite_projections p WHERE p.item_type=favorites.item_type AND p.item_id=favorites.item_id)
         AND NOT EXISTS (SELECT 1 FROM grouping_favorite_origins o WHERE o.selection_type=favorites.item_type AND o.selection_id=favorites.item_id)",
    )
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        use sqlx::Row;
        let item_type: String = row.try_get("item_type")?;
        let item_id: String = row.try_get("item_id")?;
        let created_at: i64 = row.try_get("created_at_unix_seconds")?;
        capture_favorite_origins(tx, before, &item_type, &item_id, created_at).await?;
    }
    Ok(())
}

/// Re-project favorite origins onto `after`. The caller runs this before deleting
/// the browse snapshot, in the same transaction that replaces it. Origins remain
/// untouched, which makes a split restore the original selection exactly.
pub(crate) async fn reproject_favorites(
    tx: &mut Transaction<'_, Sqlite>,
    after: &Library,
) -> Result<()> {
    let mut targets: BTreeMap<(String, String, String), BTreeSet<(String, String)>> =
        BTreeMap::new();
    for member in memberships(after) {
        targets
            .entry((
                member.track_id,
                member.role.as_str().to_string(),
                member.item_type.to_string(),
            ))
            .or_default()
            .insert((member.item_type.to_string(), member.item_id));
    }
    let origins = sqlx::query(
        "SELECT selection_type, selection_id, track_id, role, created_at_unix_seconds
         FROM grouping_favorite_origins",
    )
    .fetch_all(&mut **tx)
    .await?;
    // Only remove rows previously generated by this projection. Legacy unresolved
    // favorites have no provenance and must remain visible rather than being guessed
    // into a new group or silently discarded.
    sqlx::query(
        "DELETE FROM favorites WHERE EXISTS (
            SELECT 1 FROM grouping_favorite_projections p
            WHERE p.item_type = favorites.item_type AND p.item_id = favorites.item_id
        ) OR EXISTS (
            SELECT 1 FROM grouping_favorite_origins o
            WHERE o.selection_type=favorites.item_type AND o.selection_id=favorites.item_id
        )",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM grouping_favorite_projections")
        .execute(&mut **tx)
        .await?;
    let mut selections: BTreeMap<(String, String), (i64, BTreeSet<(String, String)>)> =
        BTreeMap::new();
    for origin in origins {
        use sqlx::Row;
        let track_id: String = origin.try_get("track_id")?;
        let role: String = origin.try_get("role")?;
        let selection_type: String = origin.try_get("selection_type")?;
        let selection_id: String = origin.try_get("selection_id")?;
        let created_at: i64 = origin.try_get("created_at_unix_seconds")?;
        let selection = selections
            .entry((selection_type.clone(), selection_id))
            .or_insert_with(|| (created_at, BTreeSet::new()));
        if let Some(items) = targets.get(&(track_id, role, selection_type)) {
            selection.1.extend(items.iter().cloned());
        }
    }
    for ((selection_type, selection_id), (created_at, mut items)) in selections {
        // An unavailable selection remains saved; it must never transfer to its
        // source slot's replacement or disappear from the listener's favorites.
        if items.is_empty() {
            items.insert((selection_type.clone(), selection_id.clone()));
        }
        for (item_type, item_id) in items {
            sqlx::query("INSERT OR IGNORE INTO favorites (item_type,item_id,created_at_unix_seconds) VALUES (?1,?2,?3)")
                .bind(&item_type).bind(&item_id).bind(created_at).execute(&mut **tx).await?;
            sqlx::query("INSERT OR IGNORE INTO grouping_favorite_projections (selection_type,selection_id,item_type,item_id) VALUES (?1,?2,?3,?4)")
                .bind(&selection_type).bind(&selection_id).bind(&item_type).bind(&item_id).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

/// Remove only the origins currently represented by this card, then rebuild the
/// projection. A merged artist can therefore be unstarred without clearing a source
/// member that has already split into a different artist card.
pub(crate) async fn clear_favorite_origins(
    tx: &mut Transaction<'_, Sqlite>,
    library: &Library,
    item_type: &str,
    item_id: &str,
) -> Result<()> {
    use sqlx::Row;
    sqlx::query("DELETE FROM favorites WHERE item_type = ?1 AND item_id = ?2")
        .bind(item_type)
        .bind(item_id)
        .execute(&mut **tx)
        .await?;
    let members = memberships(library);
    let current: BTreeSet<_> = members
        .iter()
        .filter(|member| member.item_type == item_type && member.item_id == item_id)
        .map(|member| (member.track_id.clone(), member.role.as_str().to_string()))
        .collect();
    let origins = sqlx::query(
        "SELECT selection_type, selection_id, track_id, role FROM grouping_favorite_origins",
    )
    .fetch_all(&mut **tx)
    .await?;
    for origin in origins {
        let track_id: String = origin.try_get("track_id")?;
        let role: String = origin.try_get("role")?;
        let selected = origin.try_get::<String, _>("selection_type")? == item_type
            && origin.try_get::<String, _>("selection_id")? == item_id;
        if !current.contains(&(track_id.clone(), role.clone())) && !(current.is_empty() && selected)
        {
            continue;
        }
        sqlx::query(
            "DELETE FROM grouping_favorite_origins
             WHERE selection_type = ?1 AND selection_id = ?2 AND track_id = ?3 AND role = ?4",
        )
        .bind(origin.try_get::<String, _>("selection_type")?)
        .bind(origin.try_get::<String, _>("selection_id")?)
        .bind(track_id)
        .bind(role)
        .execute(&mut **tx)
        .await?;
    }
    reproject_favorites(tx, library).await
}

/// Capture legacy favorites, then update their projection from actual before/after
/// memberships. No id is derived from a name, so MBID-keyed entities stay distinct.
pub(crate) async fn preserve_grouping_favorites(
    tx: &mut Transaction<'_, Sqlite>,
    before: &Library,
    after: &Library,
) -> Result<()> {
    seed_legacy_favorite_origins(tx, before).await?;
    reproject_favorites(tx, after).await
}

/// Snapshot each artwork record against the source album's actual track members,
/// then project it to the after-library's albums. Existing source rows are retained:
/// a later split therefore restores each original choice, while a merge uses SQLite's
/// deterministic first retained row instead of overwriting a conflicting cover.
pub(crate) async fn preserve_grouping_artwork(
    tx: &mut Transaction<'_, Sqlite>,
    before: &Library,
    after: &Library,
) -> Result<()> {
    let mut album_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for member in memberships(before)
        .into_iter()
        .filter(|member| member.role == Role::Album)
    {
        album_members
            .entry(member.item_id)
            .or_default()
            .push(member.track_id);
    }
    for row in sqlx::query("SELECT album_id,cache_key,ext,candidate_id FROM album_artwork_choices")
        .fetch_all(&mut **tx)
        .await?
    {
        use sqlx::Row;
        let album: String = row.try_get("album_id")?;
        if let Some(tracks) = album_members.get(&album) {
            for track in tracks {
                sqlx::query("INSERT OR IGNORE INTO grouping_album_artwork_origins (source_album_id,track_id,cache_key,ext,candidate_id,role) VALUES (?1,?2,?3,?4,?5,'choice')")
                .bind(&album).bind(track).bind(row.try_get::<String,_>("cache_key")?).bind(row.try_get::<String,_>("ext")?).bind(row.try_get::<Option<String>,_>("candidate_id")?).execute(&mut **tx).await?;
            }
        }
    }
    for row in sqlx::query("SELECT album_id,provider,remote_url,cache_key,ext,width,status,acquired_at_unix_seconds FROM acquired_album_artwork").fetch_all(&mut **tx).await? {
        use sqlx::Row;
        let album: String=row.try_get("album_id")?;
        if let Some(tracks)=album_members.get(&album) { for track in tracks { sqlx::query("INSERT OR IGNORE INTO grouping_album_artwork_origins (source_album_id,track_id,provider,remote_url,cache_key,ext,width,status,acquired_at_unix_seconds,role) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'acquired')")
            .bind(&album).bind(track).bind(row.try_get::<String,_>("provider")?).bind(row.try_get::<Option<String>,_>("remote_url")?).bind(row.try_get::<Option<String>,_>("cache_key")?).bind(row.try_get::<Option<String>,_>("ext")?).bind(row.try_get::<Option<i64>,_>("width")?).bind(row.try_get::<String,_>("status")?).bind(row.try_get::<i64,_>("acquired_at_unix_seconds")?).execute(&mut **tx).await?; } }
    }
    for row in
        sqlx::query("SELECT album_id,revision,absent,failures,next_attempt FROM artwork_attempts")
            .fetch_all(&mut **tx)
            .await?
    {
        use sqlx::Row;
        let album: String = row.try_get("album_id")?;
        if let Some(tracks) = album_members.get(&album) {
            for track in tracks {
                sqlx::query("INSERT OR IGNORE INTO grouping_artwork_attempt_origins (source_album_id,track_id,revision,absent,failures,next_attempt) VALUES (?1,?2,?3,?4,?5,?6)").bind(&album).bind(track).bind(row.try_get::<String,_>("revision")?).bind(row.try_get::<i64,_>("absent")?).bind(row.try_get::<i64,_>("failures")?).bind(row.try_get::<i64,_>("next_attempt")?).execute(&mut **tx).await?;
            }
        }
    }
    for row in sqlx::query("SELECT album_id,generation FROM artwork_refresh_versions")
        .fetch_all(&mut **tx)
        .await?
    {
        use sqlx::Row;
        let album: String = row.try_get("album_id")?;
        if let Some(tracks) = album_members.get(&album) {
            for track in tracks {
                sqlx::query("INSERT OR IGNORE INTO grouping_artwork_refresh_origins (source_album_id,track_id,generation) VALUES (?1,?2,?3)").bind(&album).bind(track).bind(row.try_get::<i64,_>("generation")?).execute(&mut **tx).await?;
            }
        }
    }
    let after_albums: BTreeMap<_, _> = memberships(after)
        .into_iter()
        .filter(|m| m.role == Role::Album)
        .map(|m| (m.track_id, m.item_id))
        .collect();
    for row in sqlx::query("SELECT role,track_id,cache_key,ext,candidate_id,provider,remote_url,width,status,acquired_at_unix_seconds FROM grouping_album_artwork_origins ORDER BY source_album_id,track_id").fetch_all(&mut **tx).await? {
        use sqlx::Row;
        let track: String=row.try_get("track_id")?;
        let Some(album)=after_albums.get(&track) else { continue; };
        let role: String = row.try_get("role")?;
        if role == "choice" && let (Some(key),Some(ext))=(row.try_get::<Option<String>,_>("cache_key")?,row.try_get::<Option<String>,_>("ext")?) { sqlx::query("INSERT OR IGNORE INTO album_artwork_choices (album_id,cache_key,ext,candidate_id) VALUES (?1,?2,?3,?4)").bind(album).bind(key).bind(ext).bind(row.try_get::<Option<String>,_>("candidate_id")?).execute(&mut **tx).await?; }
        if role == "acquired" && let Some(provider)=row.try_get::<Option<String>,_>("provider")? { sqlx::query("INSERT OR IGNORE INTO acquired_album_artwork (album_id,provider,remote_url,cache_key,ext,width,status,acquired_at_unix_seconds) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)").bind(album).bind(provider).bind(row.try_get::<Option<String>,_>("remote_url")?).bind(row.try_get::<Option<String>,_>("cache_key")?).bind(row.try_get::<Option<String>,_>("ext")?).bind(row.try_get::<Option<i64>,_>("width")?).bind(row.try_get::<Option<String>,_>("status")?.unwrap_or_else(||"acquired".into())).bind(row.try_get::<Option<i64>,_>("acquired_at_unix_seconds")?.unwrap_or(0)).execute(&mut **tx).await?; }
    }
    for row in sqlx::query("SELECT track_id,revision,absent,failures,next_attempt FROM grouping_artwork_attempt_origins").fetch_all(&mut **tx).await? { use sqlx::Row; let track:String=row.try_get("track_id")?; if let Some(album)=after_albums.get(&track) { sqlx::query("INSERT OR IGNORE INTO artwork_attempts (album_id,revision,absent,failures,next_attempt) VALUES (?1,?2,?3,?4,?5)").bind(album).bind(row.try_get::<String,_>("revision")?).bind(row.try_get::<i64,_>("absent")?).bind(row.try_get::<i64,_>("failures")?).bind(row.try_get::<i64,_>("next_attempt")?).execute(&mut **tx).await?; } }
    for row in sqlx::query("SELECT track_id,generation FROM grouping_artwork_refresh_origins")
        .fetch_all(&mut **tx)
        .await?
    {
        use sqlx::Row;
        let track: String = row.try_get("track_id")?;
        if let Some(album) = after_albums.get(&track) {
            sqlx::query("INSERT OR IGNORE INTO artwork_refresh_versions (album_id,generation) VALUES (?1,?2)").bind(album).bind(row.try_get::<i64,_>("generation")?).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

/// Artist images follow actual contributor roles; matching display names alone
/// cannot establish that a track performer and an album artist are the same entity.
pub(crate) async fn preserve_artist_artwork(
    tx: &mut Transaction<'_, Sqlite>,
    before: &Library,
    after: &Library,
) -> Result<()> {
    use sqlx::Row;
    let images = sqlx::query("SELECT artist_id, json_object('provider',provider,'remote_url',remote_url,'cache_key',cache_key,'ext',ext,'width',width,'status',status,'acquired_at',acquired_at_unix_seconds) AS snapshot FROM acquired_artist_artwork")
        .fetch_all(&mut **tx).await?;
    let images: BTreeMap<String, String> = images
        .into_iter()
        .map(|row| Ok((row.try_get("artist_id")?, row.try_get("snapshot")?)))
        .collect::<Result<_>>()?;
    for member in memberships(before)
        .into_iter()
        .filter(|m| m.item_type == "artist")
    {
        if let Some(snapshot) = images.get(&member.item_id) {
            sqlx::query("INSERT OR IGNORE INTO grouping_artist_artwork_origins (source_artist_id,track_id,role,snapshot_json) VALUES (?1,?2,?3,?4)")
                .bind(member.item_id).bind(member.track_id).bind(member.role.as_str()).bind(snapshot).execute(&mut **tx).await?;
        }
    }
    let targets: BTreeMap<_, _> = memberships(after)
        .into_iter()
        .filter(|m| m.item_type == "artist")
        .map(|m| ((m.track_id, m.role.as_str()), m.item_id))
        .collect();
    for row in sqlx::query("SELECT track_id,role,snapshot_json FROM grouping_artist_artwork_origins ORDER BY source_artist_id,track_id,role").fetch_all(&mut **tx).await? {
        let track: String = row.try_get("track_id")?;
        let role: String = row.try_get("role")?;
        if let Some(id) = targets.get(&(track, role.as_str())) {
            sqlx::query("INSERT OR IGNORE INTO acquired_artist_artwork (artist_id,provider,remote_url,cache_key,ext,width,status,acquired_at_unix_seconds)
                SELECT ?1,json_extract(?2,'$.provider'),json_extract(?2,'$.remote_url'),json_extract(?2,'$.cache_key'),json_extract(?2,'$.ext'),json_extract(?2,'$.width'),json_extract(?2,'$.status'),json_extract(?2,'$.acquired_at')")
                .bind(id).bind(row.try_get::<String,_>("snapshot_json")?).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

/// Clear the explicit choice represented by a current card, including retained
/// source choices that would otherwise reappear when the card is split again.
pub(crate) async fn clear_artwork_origins(
    tx: &mut Transaction<'_, Sqlite>,
    album_id: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM album_artwork_choices WHERE album_id=?1 OR album_id IN (
        SELECT source_album_id FROM grouping_album_artwork_origins WHERE role='choice'
        AND track_id IN (SELECT id FROM tracks WHERE album_id=?1))",
    )
    .bind(album_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM grouping_album_artwork_origins WHERE role='choice' AND
        (source_album_id=?1 OR track_id IN (SELECT id FROM tracks WHERE album_id=?1))",
    )
    .bind(album_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use musicata_core::{Album, Artist, ProviderMapping, Track};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    async fn database(label: &str) -> (Database, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("musicata-grouping-{label}-{unique}.db"));
        (Database::connect(&path).await.unwrap(), path)
    }

    fn library(track_artist: &str, track_artist_id: &str, album_artist_id: &str) -> Library {
        let track = Track {
            id: "version-1".into(),
            provider: ProviderMapping {
                provider_id: "p".into(),
                item_id: "a".into(),
            },
            title: "song".into(),
            artist_id: track_artist_id.into(),
            artist_name: track_artist.into(),
            album_id: "album-1".into(),
            album_title: "album".into(),
            year: None,
            track_number: None,
            disc_number: None,
            extension: "mp3".into(),
            file_size_bytes: None,
            modified_at_unix_seconds: None,
            content_hash: None,
            relative_path: "a".into(),
            stream_url: "s".into(),
            path: PathBuf::from("a"),
            added_at_unix_seconds: None,
            duration_seconds: None,
            observed_metadata: vec![],
        };
        Library {
            provider_id: "p".into(),
            source_root: "r".into(),
            tracks: vec![track],
            albums: vec![Album {
                id: "album-1".into(),
                title: "album".into(),
                artist_id: album_artist_id.into(),
                artist_name: "album artist".into(),
                year: None,
                track_count: 1,
                artwork_url: None,
                artwork_path: None,
                artwork_provider_id: None,
            }],
            artists: vec![
                Artist {
                    id: track_artist_id.into(),
                    name: track_artist.into(),
                    album_count: 0,
                    track_count: 1,
                    artwork_url: None,
                },
                Artist {
                    id: album_artist_id.into(),
                    name: "album artist".into(),
                    album_count: 1,
                    track_count: 0,
                    artwork_url: None,
                },
            ],
            scan_errors: vec![],
        }
    }

    fn two_source_artists() -> Library {
        let mut library = library("Artist A", "artist-a", "album-artist-a");
        let mut second = library.tracks[0].clone();
        second.id = "version-2".into();
        second.provider.item_id = "b".into();
        second.relative_path = "b".into();
        second.path = PathBuf::from("b");
        second.title = "song b".into();
        second.artist_id = "artist-b".into();
        second.artist_name = "Artist B".into();
        second.album_id = "album-2".into();
        second.album_title = "album b".into();
        library.tracks.push(second);
        library.albums.push(Album {
            id: "album-2".into(),
            title: "album b".into(),
            artist_id: "album-artist-b".into(),
            artist_name: "Album Artist B".into(),
            year: None,
            track_count: 1,
            artwork_url: None,
            artwork_path: None,
            artwork_provider_id: None,
        });
        library.artists.extend([
            Artist {
                id: "artist-b".into(),
                name: "Artist B".into(),
                album_count: 0,
                track_count: 1,
                artwork_url: None,
            },
            Artist {
                id: "album-artist-b".into(),
                name: "Album Artist B".into(),
                album_count: 1,
                track_count: 0,
                artwork_url: None,
            },
        ]);
        library
    }

    fn merged_artists(source: &Library) -> Library {
        let mut merged = source.clone();
        for track in &mut merged.tracks {
            track.artist_id = "artist-merged".into();
            track.artist_name = "Merged".into();
        }
        merged
            .artists
            .retain(|artist| !matches!(artist.id.as_str(), "artist-a" | "artist-b"));
        merged.artists.push(Artist {
            id: "artist-merged".into(),
            name: "Merged".into(),
            album_count: 2,
            track_count: 2,
            artwork_url: None,
        });
        merged
    }

    #[test]
    fn memberships_distinguish_track_artist_from_album_artist() {
        let members = memberships(&library(
            "guest",
            "artist-guest-mbid",
            "artist-release-mbid",
        ));
        assert!(
            members
                .iter()
                .any(|m| m.role == Role::TrackArtist && m.item_id == "artist-guest-mbid")
        );
        assert!(
            members
                .iter()
                .any(|m| m.role == Role::AlbumArtist && m.item_id == "artist-release-mbid")
        );
    }

    #[tokio::test]
    async fn merge_then_unmerge_keeps_an_artist_favorite_on_its_original_member() {
        let (database, path) = database("favorite-member").await;
        let mut source = two_source_artists();
        database.save_library(&mut source).await.unwrap();
        database
            .set_favorite("artist", "artist-a", 1)
            .await
            .unwrap();

        let mut merged = merged_artists(&source);
        database.save_library(&mut merged).await.unwrap();
        assert_eq!(
            database.favorite_ids("artist").await.unwrap(),
            ["artist-merged"]
        );

        database.save_library(&mut source).await.unwrap();
        assert_eq!(database.favorite_ids("artist").await.unwrap(), ["artist-a"]);
        assert!(
            !database
                .favorite_ids("artist")
                .await
                .unwrap()
                .contains(&"artist-b".to_string())
        );

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn removed_favorite_is_retained_until_the_listener_clears_it() {
        let (database, path) = database("removed-favorite").await;
        let mut source = two_source_artists();
        database.save_library(&mut source).await.unwrap();
        database
            .set_favorite("artist", "artist-a", 1)
            .await
            .unwrap();

        let mut only_b = source.clone();
        only_b.tracks.remove(0);
        only_b.albums.remove(0);
        only_b
            .artists
            .retain(|artist| artist.id != "artist-a" && artist.id != "album-artist-a");
        database.save_library(&mut only_b).await.unwrap();
        assert_eq!(database.favorite_ids("artist").await.unwrap(), ["artist-a"]);

        database.clear_favorite("artist", "artist-a").await.unwrap();
        assert!(database.favorite_ids("artist").await.unwrap().is_empty());

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn conflicting_manual_album_choices_clear_through_merge_and_unmerge() {
        let (database, path) = database("manual-artwork").await;
        let mut source = two_source_artists();
        database.save_library(&mut source).await.unwrap();
        database
            .set_artwork_choice("album-1", "cover-a", "png", None)
            .await
            .unwrap();
        database
            .set_artwork_choice("album-2", "cover-b", "jpg", None)
            .await
            .unwrap();

        let mut merged = source.clone();
        for track in &mut merged.tracks {
            track.album_id = "album-merged".into();
            track.album_title = "merged album".into();
        }
        merged.albums = vec![Album {
            id: "album-merged".into(),
            title: "merged album".into(),
            artist_id: "album-artist-a".into(),
            artist_name: "Album Artist A".into(),
            year: None,
            track_count: 2,
            artwork_url: None,
            artwork_path: None,
            artwork_provider_id: None,
        }];
        database.save_library(&mut merged).await.unwrap();
        assert!(
            database
                .artwork_choice("album-merged")
                .await
                .unwrap()
                .is_some()
        );

        database.clear_artwork_choice("album-merged").await.unwrap();
        database.save_library(&mut source).await.unwrap();
        assert!(database.artwork_choice("album-1").await.unwrap().is_none());
        assert!(database.artwork_choice("album-2").await.unwrap().is_none());

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn acquired_artwork_does_not_become_a_manual_choice_after_regrouping() {
        let (database, path) = database("acquired-artwork").await;
        let mut source = two_source_artists();
        database.save_library(&mut source).await.unwrap();
        sqlx::query("INSERT INTO acquired_album_artwork (album_id,provider,cache_key,ext,status,acquired_at_unix_seconds) VALUES ('album-1','test','remote-a','jpg','acquired',1)")
            .execute(&database.pool)
            .await
            .unwrap();
        let mut merged = source.clone();
        for track in &mut merged.tracks {
            track.album_id = "album-merged".into();
        }
        merged.albums = vec![source.albums[0].clone()];
        merged.albums[0].id = "album-merged".into();
        database.save_library(&mut merged).await.unwrap();
        assert!(
            database
                .artwork_choice("album-merged")
                .await
                .unwrap()
                .is_none()
        );

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn manual_artwork_projects_when_one_of_its_source_album_tracks_is_removed() {
        let (database, path) = database("artwork-member-removed").await;
        let mut source = two_source_artists();
        source.tracks[1].album_id = "album-1".into();
        source.tracks[1].album_title = "album".into();
        source.albums.remove(1);
        source.albums[0].track_count = 2;
        database.save_library(&mut source).await.unwrap();
        database
            .set_artwork_choice("album-1", "cover", "png", None)
            .await
            .unwrap();

        let mut removed_member = source.clone();
        removed_member.tracks.remove(0);
        removed_member.albums[0].track_count = 1;
        database.save_library(&mut removed_member).await.unwrap();
        assert_eq!(
            database.artwork_choice("album-1").await.unwrap(),
            Some(("cover".into(), "png".into(), None))
        );

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn manual_artwork_from_a_multi_member_album_projects_to_the_remaining_split() {
        let (database, path) = database("artwork-member-split").await;
        let mut source = two_source_artists();
        source.tracks[1].album_id = "album-1".into();
        source.tracks[1].album_title = "album".into();
        source.albums.remove(1);
        source.albums[0].track_count = 2;
        database.save_library(&mut source).await.unwrap();
        database
            .set_artwork_choice("album-1", "cover", "png", None)
            .await
            .unwrap();

        let mut split = source.clone();
        split.tracks.remove(0);
        split.tracks[0].album_id = "album-split".into();
        split.tracks[0].album_title = "split album".into();
        split.albums[0].id = "album-split".into();
        split.albums[0].title = "split album".into();
        split.albums[0].track_count = 1;
        database.save_library(&mut split).await.unwrap();
        assert_eq!(
            database.artwork_choice("album-split").await.unwrap(),
            Some(("cover".into(), "png".into(), None))
        );

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn acquired_artist_artwork_projects_only_through_its_actual_role() {
        use sqlx::Row;

        let (database, path) = database("artist-artwork-role").await;
        let mut source = two_source_artists();
        database.save_library(&mut source).await.unwrap();
        sqlx::query("INSERT INTO acquired_artist_artwork (artist_id,provider,cache_key,ext,status,acquired_at_unix_seconds) VALUES ('artist-a','test','artist-cover','jpg','acquired',1)")
            .execute(&database.pool)
            .await
            .unwrap();

        let mut merged = merged_artists(&source);
        database.save_library(&mut merged).await.unwrap();
        let ids: Vec<String> = sqlx::query(
            "SELECT artist_id FROM acquired_artist_artwork WHERE cache_key='artist-cover' ORDER BY artist_id",
        )
        .fetch_all(&database.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("artist_id").unwrap())
        .collect();
        assert_eq!(ids, ["artist-a", "artist-merged"]);
        assert!(!ids.contains(&"album-artist-a".to_string()));

        drop(database);
        let _ = std::fs::remove_file(path);
    }
}
