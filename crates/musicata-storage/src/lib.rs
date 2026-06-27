use anyhow::{Context, Result};
use musicata_core::{
    Album, Artist, BrowseFilter, BrowseIndex, BrowseTextFacet, BrowseYearFacet, Library,
    LibrarySummary, MetadataApprovalState, MetadataFieldValue, PlaybackStatus, Playlist,
    ProviderMapping, QueueItem, RadioStation, RepeatMode, ScanIssue, SearchResults, Track,
    TrackCanonicalOverride, TrackMetadataFieldObservation, TrackMetadataObservation, Zone,
    album_identity, artist_identity, regroup_library_with_overrides,
};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

/// Register the sqlite-vec extension as a global SQLite auto-extension, once per process, so
/// every connection this process opens (sqlx shares the bundled SQLite) gets the `vec0` virtual
/// table. Standard for all Musicata instances, not gated behind ML — the audio-embedding index
/// uses it, but loading it is cheap and unconditional. See `docs/decisions.md`.
fn register_sqlite_vec() {
    use std::sync::Once;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

impl Database {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        register_sqlite_vec();
        let path = path.as_ref();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            // WAL lets reads run concurrently with a writer, so a long background write
            // (the scan's full-library `save_library` rewrite — thousands of rows in one
            // transaction) no longer blocks foreground reads like `/api/library/summary`.
            // Under the old rollback (`delete`) journal, that write held an exclusive lock
            // and every concurrent request hit the 5s busy-timeout and 500'd. `NORMAL`
            // synchronous is the standard, safe pairing with WAL.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(15));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("failed to open {}", path.display()))?;
        let database = Self { pool };
        database.migrate().await?;

        Ok(database)
    }

    async fn migrate(&self) -> Result<()> {
        let version = user_version(&self.pool).await?;

        if version < 1 {
            for statement in MIGRATION_001 {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 1).await?;
        }

        if version < 2 {
            for migration in MIGRATION_002_TRACK_COLUMNS {
                ensure_column(
                    &self.pool,
                    "tracks",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 2).await?;
        }

        if version < 3 {
            for statement in MIGRATION_003 {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 3).await?;
        }

        if version < 4 {
            for migration in MIGRATION_004_METADATA_OBSERVATION_COLUMNS {
                ensure_column(
                    &self.pool,
                    "track_metadata_observations",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 4).await?;
        }

        if version < 5 {
            for migration in MIGRATION_005_METADATA_OBSERVATION_VALUE_COLUMNS {
                ensure_column(
                    &self.pool,
                    "track_metadata_observations",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 5).await?;
        }

        if version < 6 {
            for statement in MIGRATION_006_METADATA_FIELD_OBSERVATIONS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 6).await?;
        }

        if version < 7 {
            for migration in MIGRATION_007_TRACK_DISC_NUMBER {
                ensure_column(
                    &self.pool,
                    "tracks",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 7).await?;
        }

        if version < 8 {
            for migration in MIGRATION_008_TRACK_ADDED_AT {
                ensure_column(
                    &self.pool,
                    "tracks",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 8).await?;
        }

        if version < 9 {
            for statement in MIGRATION_009_FTS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 9).await?;
        }

        if version < 10 {
            for statement in MIGRATION_010_PLAYERS_AND_ZONES {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 10).await?;
        }

        if version < 11 {
            for statement in MIGRATION_011_LISTENS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 11).await?;
        }

        if version < 12 {
            for migration in MIGRATION_012_TRACK_DURATION {
                ensure_column(
                    &self.pool,
                    "tracks",
                    migration.column,
                    migration.alter_statement,
                )
                .await?;
            }
            set_user_version(&self.pool, 12).await?;
        }

        if version < 13 {
            for statement in MIGRATION_013_PLAYLISTS_AND_FAVORITES {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 13).await?;
        }

        if version < 14 {
            for statement in MIGRATION_014_RADIO_STATIONS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 14).await?;
        }

        if version < 15 {
            for statement in MIGRATION_015_SOURCES {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 15).await?;
        }

        if version < 16 {
            for statement in MIGRATION_016_ACTIVITIES {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 16).await?;
        }

        if version < 17 {
            for statement in MIGRATION_017_PLAYER_QUEUE {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 17).await?;
        }

        if version < 18 {
            for statement in MIGRATION_018_ZONE_QUEUE {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 18).await?;
        }

        if version < 19 {
            for statement in MIGRATION_019_ACQUIRED_ARTWORK {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 19).await?;
        }

        if version < 20 {
            for statement in MIGRATION_020_SETTINGS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 20).await?;
        }

        if version < 21 {
            for statement in MIGRATION_021_TRACK_FINGERPRINT {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 21).await?;
        }

        if version < 22 {
            for statement in MIGRATION_022_TRACK_MUSICBRAINZ_METADATA {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 22).await?;
        }

        if version < 23 {
            for statement in MIGRATION_023_LISTEN_EVENT_KIND {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 23).await?;
        }

        if version < 24 {
            for statement in MIGRATION_024_ARTIST_ARTWORK {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 24).await?;
        }

        if version < 25 {
            for statement in MIGRATION_025_ARTIST_ALIASES {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 25).await?;
        }

        if version < 26 {
            for statement in MIGRATION_026_TRACK_LOUDNESS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 26).await?;
        }

        if version < 27 {
            for statement in MIGRATION_027_SIMILARITY_CACHE {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 27).await?;
        }

        if version < 28 {
            for statement in MIGRATION_028_USERS_AND_SESSIONS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 28).await?;
        }

        if version < 29 {
            for statement in MIGRATION_029_ALBUM_LOUDNESS {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 29).await?;
        }

        if version < 30 {
            for statement in MIGRATION_030_PLAYER_AUTH_TOKEN {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 30).await?;
        }

        if version < 31 {
            for statement in MIGRATION_031_AUDIO_EMBEDDING {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 31).await?;
        }

        if version < 32 {
            for statement in MIGRATION_032_SCROBBLE_QUEUE {
                sqlx::query(statement).execute(&self.pool).await?;
            }
            set_user_version(&self.pool, 32).await?;
        }

        Ok(())
    }

    pub async fn save_library(&self, library: &mut Library) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let result = async {
            // Preserve each track's original "added at" timestamp across the full
            // delete/re-insert by keying on (provider id, provider item id) — the
            // item id alone can collide once more than one source is merged in.
            let mut existing_added_at: BTreeMap<(String, String), Option<i64>> = BTreeMap::new();
            let existing_rows =
                sqlx::query("SELECT provider_id, provider_item_id, added_at_unix_seconds FROM tracks")
                    .fetch_all(&mut *conn)
                    .await?;
            for row in existing_rows {
                existing_added_at.insert(
                    (row.try_get("provider_id")?, row.try_get("provider_item_id")?),
                    row.try_get("added_at_unix_seconds")?,
                );
            }
            let now = now_unix_seconds();
            for track in &mut library.tracks {
                let key = (
                    track.provider.provider_id.clone(),
                    track.provider.item_id.clone(),
                );
                let added_at = track
                    .added_at_unix_seconds
                    .or_else(|| existing_added_at.get(&key).copied().flatten())
                    .unwrap_or(now);
                track.added_at_unix_seconds = Some(added_at);
            }

            sqlx::query("DELETE FROM track_metadata_field_observations")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM track_metadata_observations")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM tracks").execute(&mut *conn).await?;
            sqlx::query("DELETE FROM scan_errors")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM albums").execute(&mut *conn).await?;
            sqlx::query("DELETE FROM artists").execute(&mut *conn).await?;
            sqlx::query("DELETE FROM providers")
                .execute(&mut *conn)
                .await?;

            sqlx::query(
                "INSERT INTO providers (id, source_root, scanned_at) VALUES (?1, ?2, ?3)",
            )
            .bind(&library.provider_id)
            .bind(&library.source_root)
            .bind(now_unix_seconds())
            .execute(&mut *conn)
            .await?;

            for artist in &library.artists {
                sqlx::query(
                    "INSERT INTO artists (id, name, album_count, track_count) VALUES (?1, ?2, ?3, ?4)",
                )
                .bind(&artist.id)
                .bind(&artist.name)
                .bind(artist.album_count as i64)
                .bind(artist.track_count as i64)
                .execute(&mut *conn)
                .await?;
            }

            for album in &library.albums {
                sqlx::query(
                    "INSERT INTO albums (id, title, artist_id, artist_name, year, track_count, artwork_url, artwork_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .bind(&album.id)
                .bind(&album.title)
                .bind(&album.artist_id)
                .bind(&album.artist_name)
                .bind(album.year.map(i64::from))
                .bind(album.track_count as i64)
                .bind(&album.artwork_url)
                .bind(album.artwork_path.as_ref().map(path_to_string))
                .execute(&mut *conn)
                .await?;
            }

            for track in &library.tracks {
                sqlx::query(
                    "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, disc_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path, added_at_unix_seconds, duration_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                )
                .bind(&track.id)
                .bind(&track.provider.provider_id)
                .bind(&track.provider.item_id)
                .bind(&track.title)
                .bind(&track.artist_id)
                .bind(&track.artist_name)
                .bind(&track.album_id)
                .bind(&track.album_title)
                .bind(track.year.map(i64::from))
                .bind(track.track_number.map(i64::from))
                .bind(track.disc_number.map(i64::from))
                .bind(&track.extension)
                .bind(track.file_size_bytes.map(u64_to_i64).transpose()?)
                .bind(track.modified_at_unix_seconds)
                .bind(&track.content_hash)
                .bind(&track.relative_path)
                .bind(&track.stream_url)
                .bind(path_to_string(&track.path))
                .bind(track.added_at_unix_seconds)
                .bind(track.duration_seconds)
                .execute(&mut *conn)
                .await?;

                for observation in &track.observed_metadata {
                    let insert_result = sqlx::query(
                        "INSERT INTO track_metadata_observations (
                            track_id, source, confidence, observed_at_unix_seconds, approval_state,
                            title, artist_name, album_artist_name, album_title, recording_date,
                            year, track_number, track_total, disc_number, disc_total, genres,
                            composers, lyrics, musicbrainz_recording_id, musicbrainz_track_id,
                            musicbrainz_release_id, musicbrainz_release_group_id,
                            musicbrainz_artist_id, musicbrainz_release_artist_id, isrc,
                            embedded_artwork_count
                        ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                            ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                            ?25, ?26
                        )",
                    )
                    .bind(&track.id)
                    .bind(&observation.source)
                    .bind(f64::from(observation.confidence))
                    .bind(observation.observed_at_unix_seconds)
                    .bind(approval_state_to_str(&observation.approval_state))
                    .bind(&observation.title)
                    .bind(&observation.artist_name)
                    .bind(&observation.album_artist_name)
                    .bind(&observation.album_title)
                    .bind(&observation.recording_date)
                    .bind(observation.year.map(i64::from))
                    .bind(observation.track_number.map(i64::from))
                    .bind(observation.track_total.map(i64::from))
                    .bind(observation.disc_number.map(i64::from))
                    .bind(observation.disc_total.map(i64::from))
                    .bind(metadata_values_to_json(&observation.genres)?)
                    .bind(metadata_values_to_json(&observation.composers)?)
                    .bind(&observation.lyrics)
                    .bind(&observation.musicbrainz_recording_id)
                    .bind(&observation.musicbrainz_track_id)
                    .bind(&observation.musicbrainz_release_id)
                    .bind(&observation.musicbrainz_release_group_id)
                    .bind(&observation.musicbrainz_artist_id)
                    .bind(&observation.musicbrainz_release_artist_id)
                    .bind(&observation.isrc)
                    .bind(usize_to_i64(observation.embedded_artwork_count)?)
                    .execute(&mut *conn)
                    .await?;
                    let observation_id = insert_result.last_insert_rowid();

                    for field_observation in observation.effective_field_observations() {
                        sqlx::query(
                            "INSERT INTO track_metadata_field_observations (
                                observation_id, track_id, source, field_name, value_json, confidence,
                                observed_at_unix_seconds, approval_state
                            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        )
                        .bind(observation_id)
                        .bind(&track.id)
                        .bind(&field_observation.source)
                        .bind(&field_observation.field_name)
                        .bind(metadata_field_value_to_json(&field_observation.value)?)
                        .bind(f64::from(field_observation.confidence))
                        .bind(field_observation.observed_at_unix_seconds)
                        .bind(approval_state_to_str(&field_observation.approval_state))
                        .execute(&mut *conn)
                        .await?;
                    }
                }
            }

            for issue in &library.scan_errors {
                sqlx::query("INSERT INTO scan_errors (path, message) VALUES (?1, ?2)")
                    .bind(&issue.path)
                    .bind(&issue.message)
                    .execute(&mut *conn)
                    .await?;
            }

            Ok::<(), anyhow::Error>(())
        }
        .await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    pub async fn load_library(&self) -> Result<Option<Library>> {
        let Some(provider) =
            sqlx::query("SELECT id, source_root FROM providers ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(None);
        };

        let provider_id: String = provider.try_get("id")?;
        let source_root: String = provider.try_get("source_root")?;

        let artist_rows =
            sqlx::query("SELECT id, name, album_count, track_count FROM artists ORDER BY name")
                .fetch_all(&self.pool)
                .await?;
        let album_rows = sqlx::query(
            "SELECT id, title, artist_id, artist_name, year, track_count, artwork_url, artwork_path FROM albums ORDER BY artist_name, year, title",
        )
        .fetch_all(&self.pool)
        .await?;
        let track_rows = sqlx::query(
            "SELECT id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, disc_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path, added_at_unix_seconds, duration_seconds FROM tracks ORDER BY artist_name, year, album_title, disc_number, track_number, title",
        )
        .fetch_all(&self.pool)
        .await?;
        let observation_rows = sqlx::query(
            "SELECT id, track_id, source, confidence, observed_at_unix_seconds, approval_state,
                title, artist_name, album_artist_name, album_title, recording_date, year,
                track_number, track_total, disc_number, disc_total, genres, composers, lyrics,
                musicbrainz_recording_id, musicbrainz_track_id, musicbrainz_release_id,
                musicbrainz_release_group_id, musicbrainz_artist_id, musicbrainz_release_artist_id,
                isrc, embedded_artwork_count
            FROM track_metadata_observations ORDER BY track_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let field_observation_rows = sqlx::query(
            "SELECT observation_id, source, field_name, value_json, confidence,
                observed_at_unix_seconds, approval_state
            FROM track_metadata_field_observations ORDER BY observation_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let scan_error_rows = sqlx::query("SELECT path, message FROM scan_errors ORDER BY id")
            .fetch_all(&self.pool)
            .await?;

        let mut artists = Vec::with_capacity(artist_rows.len());
        for row in artist_rows {
            artists.push(Artist {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                album_count: i64_to_usize(row.try_get("album_count")?, "album_count")?,
                track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
                // The in-memory library snapshot (used for scan/merge) doesn't carry the
                // served artwork URL — it lives in the DB column, re-applied post-scan.
                artwork_url: None,
            });
        }

        let mut albums = Vec::with_capacity(album_rows.len());
        for row in album_rows {
            albums.push(Album {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                artist_id: row.try_get("artist_id")?,
                artist_name: row.try_get("artist_name")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
                artwork_url: row.try_get("artwork_url")?,
                artwork_path: row
                    .try_get::<Option<String>, _>("artwork_path")?
                    .map(PathBuf::from),
            });
        }

        let mut field_observations_by_observation: BTreeMap<
            i64,
            Vec<TrackMetadataFieldObservation>,
        > = BTreeMap::new();
        for row in field_observation_rows {
            let source: String = row.try_get("source")?;
            let observed_at_unix_seconds: i64 = row.try_get("observed_at_unix_seconds")?;
            field_observations_by_observation
                .entry(row.try_get("observation_id")?)
                .or_default()
                .push(TrackMetadataFieldObservation {
                    source,
                    field_name: row.try_get("field_name")?,
                    value: metadata_field_value_from_json(row.try_get("value_json")?)?,
                    confidence: f64_to_f32(row.try_get("confidence")?, "confidence")?,
                    observed_at_unix_seconds,
                    approval_state: approval_state_from_str(row.try_get("approval_state")?),
                });
        }

        let mut observations_by_track: BTreeMap<String, Vec<TrackMetadataObservation>> =
            BTreeMap::new();
        for row in observation_rows {
            let observation_id: i64 = row.try_get("id")?;
            let track_id: String = row.try_get("track_id")?;
            let source: String = row.try_get("source")?;
            let observed_at_unix_seconds: i64 = row.try_get("observed_at_unix_seconds")?;
            let field_observations = field_observations_by_observation
                .remove(&observation_id)
                .unwrap_or_default();
            let mut observation = TrackMetadataObservation {
                source,
                confidence: f64_to_f32(row.try_get("confidence")?, "confidence")?,
                observed_at_unix_seconds,
                approval_state: approval_state_from_str(row.try_get("approval_state")?),
                field_observations,
                title: row.try_get("title")?,
                artist_name: row.try_get("artist_name")?,
                album_artist_name: row.try_get("album_artist_name")?,
                album_title: row.try_get("album_title")?,
                recording_date: row.try_get("recording_date")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
                track_total: optional_i64_to_u16(row.try_get("track_total")?, "track_total")?,
                disc_number: optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?,
                disc_total: optional_i64_to_u16(row.try_get("disc_total")?, "disc_total")?,
                genres: metadata_values_from_json(row.try_get("genres")?, "genres")?,
                composers: metadata_values_from_json(row.try_get("composers")?, "composers")?,
                lyrics: row.try_get("lyrics")?,
                musicbrainz_recording_id: row.try_get("musicbrainz_recording_id")?,
                musicbrainz_track_id: row.try_get("musicbrainz_track_id")?,
                musicbrainz_release_id: row.try_get("musicbrainz_release_id")?,
                musicbrainz_release_group_id: row.try_get("musicbrainz_release_group_id")?,
                musicbrainz_artist_id: row.try_get("musicbrainz_artist_id")?,
                musicbrainz_release_artist_id: row.try_get("musicbrainz_release_artist_id")?,
                isrc: row.try_get("isrc")?,
                embedded_artwork_count: i64_to_usize(
                    row.try_get("embedded_artwork_count")?,
                    "embedded_artwork_count",
                )?,
            };
            if observation.field_observations.is_empty() {
                observation.field_observations = observation.effective_field_observations();
            }
            observations_by_track
                .entry(track_id)
                .or_default()
                .push(observation);
        }

        let mut tracks = Vec::with_capacity(track_rows.len());
        for row in track_rows {
            let id: String = row.try_get("id")?;
            tracks.push(Track {
                observed_metadata: observations_by_track.remove(&id).unwrap_or_default(),
                id,
                provider: ProviderMapping {
                    provider_id: row.try_get("provider_id")?,
                    item_id: row.try_get("provider_item_id")?,
                },
                title: row.try_get("title")?,
                artist_id: row.try_get("artist_id")?,
                artist_name: row.try_get("artist_name")?,
                album_id: row.try_get("album_id")?,
                album_title: row.try_get("album_title")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
                disc_number: optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?,
                extension: row.try_get("extension")?,
                file_size_bytes: optional_i64_to_u64(
                    row.try_get("file_size_bytes")?,
                    "file_size_bytes",
                )?,
                duration_seconds: row.try_get("duration_seconds")?,
                modified_at_unix_seconds: row.try_get("modified_at_unix_seconds")?,
                content_hash: row.try_get("content_hash")?,
                relative_path: row.try_get("relative_path")?,
                stream_url: row.try_get("stream_url")?,
                added_at_unix_seconds: row.try_get("added_at_unix_seconds")?,
                path: PathBuf::from(row.try_get::<String, _>("path")?),
            });
        }

        let mut scan_errors = Vec::with_capacity(scan_error_rows.len());
        for row in scan_error_rows {
            scan_errors.push(ScanIssue {
                path: row.try_get("path")?,
                message: row.try_get("message")?,
            });
        }

        Ok(Some(Library {
            provider_id,
            source_root,
            artists,
            albums,
            tracks,
            scan_errors,
        }))
    }

    pub async fn detect_library_changes(&self, scanned: &Library) -> Result<LibraryChangeSet> {
        let rows = sqlx::query(
            "SELECT id, provider_item_id, file_size_bytes, modified_at_unix_seconds, content_hash FROM tracks ORDER BY provider_item_id",
        )
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() && !scanned.tracks.is_empty() {
            return Ok(LibraryChangeSet {
                added: scanned.tracks.len(),
                removed: 0,
                modified: 0,
            });
        }

        let observation_rows = sqlx::query(
            "SELECT id, track_id, source, confidence, observed_at_unix_seconds, approval_state,
                title, artist_name, album_artist_name, album_title, recording_date, year,
                track_number, track_total, disc_number, disc_total, genres, composers, lyrics,
                musicbrainz_recording_id, musicbrainz_track_id, musicbrainz_release_id,
                musicbrainz_release_group_id, musicbrainz_artist_id, musicbrainz_release_artist_id,
                isrc, embedded_artwork_count
            FROM track_metadata_observations ORDER BY track_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let field_observation_rows = sqlx::query(
            "SELECT observation_id, source, field_name, value_json, confidence,
                observed_at_unix_seconds, approval_state
            FROM track_metadata_field_observations ORDER BY observation_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut field_observations_by_observation: BTreeMap<
            i64,
            Vec<TrackMetadataFieldObservation>,
        > = BTreeMap::new();
        for row in field_observation_rows {
            let source: String = row.try_get("source")?;
            let observed_at_unix_seconds: i64 = row.try_get("observed_at_unix_seconds")?;
            field_observations_by_observation
                .entry(row.try_get("observation_id")?)
                .or_default()
                .push(TrackMetadataFieldObservation {
                    source,
                    field_name: row.try_get("field_name")?,
                    value: metadata_field_value_from_json(row.try_get("value_json")?)?,
                    confidence: f64_to_f32(row.try_get("confidence")?, "confidence")?,
                    observed_at_unix_seconds,
                    approval_state: approval_state_from_str(row.try_get("approval_state")?),
                });
        }
        let mut stored_observations: BTreeMap<String, String> = BTreeMap::new();
        for row in observation_rows {
            let observation_id: i64 = row.try_get("id")?;
            let track_id: String = row.try_get("track_id")?;
            let mut observation = TrackMetadataObservation {
                source: row.try_get("source")?,
                confidence: f64_to_f32(row.try_get("confidence")?, "confidence")?,
                observed_at_unix_seconds: row.try_get("observed_at_unix_seconds")?,
                approval_state: approval_state_from_str(row.try_get("approval_state")?),
                field_observations: field_observations_by_observation
                    .remove(&observation_id)
                    .unwrap_or_default(),
                title: row.try_get("title")?,
                artist_name: row.try_get("artist_name")?,
                album_artist_name: row.try_get("album_artist_name")?,
                album_title: row.try_get("album_title")?,
                recording_date: row.try_get("recording_date")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
                track_total: optional_i64_to_u16(row.try_get("track_total")?, "track_total")?,
                disc_number: optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?,
                disc_total: optional_i64_to_u16(row.try_get("disc_total")?, "disc_total")?,
                genres: metadata_values_from_json(row.try_get("genres")?, "genres")?,
                composers: metadata_values_from_json(row.try_get("composers")?, "composers")?,
                lyrics: row.try_get("lyrics")?,
                musicbrainz_recording_id: row.try_get("musicbrainz_recording_id")?,
                musicbrainz_track_id: row.try_get("musicbrainz_track_id")?,
                musicbrainz_release_id: row.try_get("musicbrainz_release_id")?,
                musicbrainz_release_group_id: row.try_get("musicbrainz_release_group_id")?,
                musicbrainz_artist_id: row.try_get("musicbrainz_artist_id")?,
                musicbrainz_release_artist_id: row.try_get("musicbrainz_release_artist_id")?,
                isrc: row.try_get("isrc")?,
                embedded_artwork_count: i64_to_usize(
                    row.try_get("embedded_artwork_count")?,
                    "embedded_artwork_count",
                )?,
            };
            if observation.field_observations.is_empty() {
                observation.field_observations = observation.effective_field_observations();
            }
            stored_observations.entry(track_id).or_default().push_str(
                &metadata_observations_fingerprint(std::slice::from_ref(&observation)),
            );
        }

        let mut stored = BTreeMap::new();
        for row in rows {
            let track_id: String = row.try_get("id")?;
            stored.insert(
                row.try_get::<String, _>("provider_item_id")?,
                TrackFingerprint {
                    file_size_bytes: optional_i64_to_u64(
                        row.try_get("file_size_bytes")?,
                        "file_size_bytes",
                    )?,
                    modified_at_unix_seconds: row.try_get("modified_at_unix_seconds")?,
                    content_hash: row.try_get("content_hash")?,
                    metadata_observations: stored_observations
                        .remove(&track_id)
                        .unwrap_or_default(),
                },
            );
        }

        let scanned: BTreeMap<_, _> = scanned
            .tracks
            .iter()
            .map(|track| {
                (
                    track.provider.item_id.clone(),
                    TrackFingerprint {
                        file_size_bytes: track.file_size_bytes,
                        modified_at_unix_seconds: track.modified_at_unix_seconds,
                        content_hash: track.content_hash.clone(),
                        metadata_observations: metadata_observations_fingerprint(
                            &track.observed_metadata,
                        ),
                    },
                )
            })
            .collect();

        let stored_ids: BTreeSet<_> = stored.keys().collect();
        let scanned_ids: BTreeSet<_> = scanned.keys().collect();
        let added = scanned_ids.difference(&stored_ids).count();
        let removed = stored_ids.difference(&scanned_ids).count();
        let modified = stored_ids
            .intersection(&scanned_ids)
            .filter(|id| stored.get(**id) != scanned.get(**id))
            .count();

        Ok(LibraryChangeSet {
            added,
            removed,
            modified,
        })
    }

    /// Full-text search across artists, albums, and tracks via the FTS5 indexes,
    /// ranked best-match-first and capped at `limit` per entity type. Tracks are
    /// returned without their observation provenance (`observed_metadata` is empty);
    /// clients needing that fetch the track or album detail.
    /// Full-text search across artists, albums, and tracks. `limit`/`offset` page each
    /// result set identically (artists/albums are short, so later pages there just
    /// return empty while tracks keep coming — the UI infinite-scrolls tracks).
    pub async fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchResults> {
        let Some(match_query) = fts_match_query(query) else {
            return Ok(SearchResults::default());
        };
        let limit = limit as i64;
        let offset = offset as i64;

        let artist_rows = sqlx::query(
            "SELECT a.id, a.name, a.album_count, a.track_count
             FROM artists_fts f JOIN artists a ON a.rowid = f.rowid
             WHERE artists_fts MATCH ?1 ORDER BY rank LIMIT ?2 OFFSET ?3",
        )
        .bind(&match_query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut artists = Vec::with_capacity(artist_rows.len());
        for row in artist_rows {
            artists.push(Artist {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                album_count: i64_to_usize(row.try_get("album_count")?, "album_count")?,
                track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
                // The in-memory library snapshot (used for scan/merge) doesn't carry the
                // served artwork URL — it lives in the DB column, re-applied post-scan.
                artwork_url: None,
            });
        }

        let album_rows = sqlx::query(
            "SELECT a.id, a.title, a.artist_id, a.artist_name, a.year, a.track_count, a.artwork_url, a.artwork_path
             FROM albums_fts f JOIN albums a ON a.rowid = f.rowid
             WHERE albums_fts MATCH ?1 ORDER BY rank LIMIT ?2 OFFSET ?3",
        )
        .bind(&match_query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut albums = Vec::with_capacity(album_rows.len());
        for row in album_rows {
            albums.push(Album {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                artist_id: row.try_get("artist_id")?,
                artist_name: row.try_get("artist_name")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
                artwork_url: row.try_get("artwork_url")?,
                artwork_path: row
                    .try_get::<Option<String>, _>("artwork_path")?
                    .map(PathBuf::from),
            });
        }

        let track_rows = sqlx::query(
            "SELECT t.id, t.provider_id, t.provider_item_id, t.title, t.artist_id, t.artist_name,
                    t.album_id, t.album_title, t.year, t.track_number, t.disc_number, t.extension,
                    t.file_size_bytes, t.modified_at_unix_seconds, t.content_hash, t.relative_path,
                    t.stream_url, t.added_at_unix_seconds, t.duration_seconds, t.path
             FROM tracks_fts f JOIN tracks t ON t.rowid = f.rowid
             WHERE tracks_fts MATCH ?1 ORDER BY rank LIMIT ?2 OFFSET ?3",
        )
        .bind(&match_query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut tracks = Vec::with_capacity(track_rows.len());
        for row in track_rows {
            tracks.push(Track {
                id: row.try_get("id")?,
                provider: ProviderMapping {
                    provider_id: row.try_get("provider_id")?,
                    item_id: row.try_get("provider_item_id")?,
                },
                observed_metadata: Vec::new(),
                title: row.try_get("title")?,
                artist_id: row.try_get("artist_id")?,
                artist_name: row.try_get("artist_name")?,
                album_id: row.try_get("album_id")?,
                album_title: row.try_get("album_title")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
                disc_number: optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?,
                extension: row.try_get("extension")?,
                file_size_bytes: optional_i64_to_u64(
                    row.try_get("file_size_bytes")?,
                    "file_size_bytes",
                )?,
                duration_seconds: row.try_get("duration_seconds")?,
                modified_at_unix_seconds: row.try_get("modified_at_unix_seconds")?,
                content_hash: row.try_get("content_hash")?,
                relative_path: row.try_get("relative_path")?,
                stream_url: row.try_get("stream_url")?,
                added_at_unix_seconds: row.try_get("added_at_unix_seconds")?,
                path: PathBuf::from(row.try_get::<String, _>("path")?),
            });
        }

        Ok(SearchResults {
            query: query.to_string(),
            artists,
            albums,
            tracks,
        })
    }

    /// Provider identity and entity counts, read directly from the database.
    pub async fn summary(&self) -> Result<Option<LibrarySummary>> {
        let Some(provider) =
            sqlx::query("SELECT id, source_root FROM providers ORDER BY rowid LIMIT 1")
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(None);
        };
        let counts = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM artists) AS artist_count,
                (SELECT COUNT(*) FROM albums) AS album_count,
                (SELECT COUNT(*) FROM tracks) AS track_count",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Some(LibrarySummary {
            provider_id: provider.try_get("id")?,
            source_root: provider.try_get("source_root")?,
            artist_count: i64_to_usize(counts.try_get("artist_count")?, "artist_count")?,
            album_count: i64_to_usize(counts.try_get("album_count")?, "album_count")?,
            track_count: i64_to_usize(counts.try_get("track_count")?, "track_count")?,
        }))
    }

    pub async fn list_artists(
        &self,
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Artist>, usize)> {
        let total = i64_to_usize(
            sqlx::query("SELECT COUNT(*) AS n FROM artists")
                .fetch_one(&self.pool)
                .await?
                .try_get("n")?,
            "total",
        )?;
        let sql = format!(
            "SELECT id, name, album_count, track_count, artwork_url FROM artists ORDER BY {} LIMIT ?1 OFFSET ?2",
            artist_order_clause(sort)
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        let mut artists = Vec::with_capacity(rows.len());
        for row in rows {
            artists.push(artist_from_row(&row)?);
        }
        Ok((artists, total))
    }

    pub async fn list_albums(
        &self,
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Album>, usize)> {
        let total = i64_to_usize(
            sqlx::query("SELECT COUNT(*) AS n FROM albums")
                .fetch_one(&self.pool)
                .await?
                .try_get("n")?,
            "total",
        )?;
        let sql = format!(
            "SELECT {ALBUM_COLUMNS} FROM albums ORDER BY {} LIMIT ?1 OFFSET ?2",
            album_order_clause(sort)
        );
        let rows = sqlx::query(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        let mut albums = Vec::with_capacity(rows.len());
        for row in rows {
            albums.push(album_from_row(&row)?);
        }
        Ok((albums, total))
    }

    /// Like [`list_albums`], but restricted to albums that have at least one track
    /// matching the browse filter — so browsing by genre/year/composer/folder narrows
    /// the album grid the same way it narrows the track list. Same filter semantics as
    /// [`list_tracks`] (genre/composer match any observation's JSON array; folder
    /// matches a folder or any nested one).
    pub async fn list_albums_filtered(
        &self,
        filter: &BrowseFilter,
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Album>, usize)> {
        let year = filter.year.map(i64::from);
        let genre = filter
            .genre
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let composer = filter
            .composer
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let folder_prefix = filter
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|folder| format!("{}/", folder.trim_end_matches('/')));
        let tag = filter
            .tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // An album is in the result iff it has a track matching the filter.
        const MATCH: &str = "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id
             AND (?1 IS NULL OR t.year = ?1)
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM track_metadata_observations o, json_each(o.genres)
                  WHERE o.track_id = t.id AND json_each.value = ?2 COLLATE NOCASE))
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM track_metadata_observations o, json_each(o.composers)
                  WHERE o.track_id = t.id AND json_each.value = ?3 COLLATE NOCASE))
             AND (?4 IS NULL OR substr(t.relative_path, 1, length(?4)) = ?4)
             AND (?5 IS NULL OR EXISTS (SELECT 1 FROM track_features f, json_each(f.tags_json) je
                  WHERE f.track_id = t.id AND json_extract(je.value, '$.label') = ?5 COLLATE NOCASE)))";

        let count_sql = format!("SELECT COUNT(*) AS n FROM albums a WHERE {MATCH}");
        let total = i64_to_usize(
            sqlx::query(&count_sql)
                .bind(year)
                .bind(genre)
                .bind(composer)
                .bind(&folder_prefix)
                .bind(tag)
                .fetch_one(&self.pool)
                .await?
                .try_get("n")?,
            "total",
        )?;

        let sql = format!(
            "SELECT {ALBUM_COLUMNS} FROM albums a WHERE {MATCH} ORDER BY {} LIMIT ?6 OFFSET ?7",
            album_order_clause(sort)
        );
        let rows = sqlx::query(&sql)
            .bind(year)
            .bind(genre)
            .bind(composer)
            .bind(&folder_prefix)
            .bind(tag)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        let mut albums = Vec::with_capacity(rows.len());
        for row in rows {
            albums.push(album_from_row(&row)?);
        }
        Ok((albums, total))
    }

    /// Paginated, sorted, filtered track listing. Tracks are returned without
    /// observation provenance (`observed_metadata` is empty); the metadata-review
    /// endpoint serves that. Genre/composer filters match any observation via the
    /// stored JSON arrays; the folder filter matches a folder or any nested folder.
    pub async fn list_tracks(
        &self,
        filter: &BrowseFilter,
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Track>, usize)> {
        let year = filter.year.map(i64::from);
        let genre = filter
            .genre
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let composer = filter
            .composer
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let folder_prefix = filter
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|folder| format!("{}/", folder.trim_end_matches('/')));
        let tag = filter
            .tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        const FILTER: &str = "(?1 IS NULL OR t.year = ?1)
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM track_metadata_observations o, json_each(o.genres)
                  WHERE o.track_id = t.id AND json_each.value = ?2 COLLATE NOCASE))
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM track_metadata_observations o, json_each(o.composers)
                  WHERE o.track_id = t.id AND json_each.value = ?3 COLLATE NOCASE))
             AND (?4 IS NULL OR substr(t.relative_path, 1, length(?4)) = ?4)
             AND (?5 IS NULL OR EXISTS (SELECT 1 FROM track_features f, json_each(f.tags_json) je
                  WHERE f.track_id = t.id AND json_extract(je.value, '$.label') = ?5 COLLATE NOCASE))";

        let count_sql = format!("SELECT COUNT(*) AS n FROM tracks t WHERE {FILTER}");
        let total = i64_to_usize(
            sqlx::query(&count_sql)
                .bind(year)
                .bind(genre)
                .bind(composer)
                .bind(&folder_prefix)
                .bind(tag)
                .fetch_one(&self.pool)
                .await?
                .try_get("n")?,
            "total",
        )?;

        let sql = format!(
            "SELECT {} FROM tracks t WHERE {FILTER} ORDER BY {} LIMIT ?6 OFFSET ?7",
            track_columns("t"),
            track_order_clause(sort)
        );
        let rows = sqlx::query(&sql)
            .bind(year)
            .bind(genre)
            .bind(composer)
            .bind(&folder_prefix)
            .bind(tag)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        let mut tracks = Vec::with_capacity(rows.len());
        for row in rows {
            tracks.push(track_from_row(&row)?);
        }
        Ok((tracks, total))
    }

    pub async fn artist(&self, id: &str) -> Result<Option<Artist>> {
        let row =
            sqlx::query("SELECT id, name, album_count, track_count, artwork_url FROM artists WHERE id = ?1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|row| artist_from_row(&row)).transpose()
    }

    pub async fn album(&self, id: &str) -> Result<Option<Album>> {
        let sql = format!("SELECT {ALBUM_COLUMNS} FROM albums WHERE id = ?1");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| album_from_row(&row)).transpose()
    }

    /// The artwork URL for an album, if any.
    pub async fn album_artwork_url(&self, album_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT artwork_url FROM albums WHERE id = ?1")
            .bind(album_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row.try_get("artwork_url")?),
            None => Ok(None),
        }
    }

    /// A random sample of tracks, newest randomization each call. Used by the
    /// OpenSubsonic `getRandomSongs` endpoint.
    pub async fn random_tracks(&self, limit: i64) -> Result<Vec<Track>> {
        let columns = track_columns("t");
        let sql = format!("SELECT {columns} FROM tracks t ORDER BY RANDOM() LIMIT ?1");
        let rows = sqlx::query(&sql).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(track_from_row).collect()
    }

    /// The most confident stored lyrics for a track, if any (from embedded tags or a
    /// sidecar file). Used by the OpenSubsonic lyrics endpoints.
    pub async fn track_lyrics(&self, track_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT lyrics FROM track_metadata_observations
             WHERE track_id = ?1 AND lyrics IS NOT NULL AND lyrics != ''
             ORDER BY confidence DESC LIMIT 1",
        )
        .bind(track_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(row.try_get("lyrics")?),
            None => Ok(None),
        }
    }

    pub async fn track(&self, id: &str) -> Result<Option<Track>> {
        let sql = format!(
            "SELECT {} FROM tracks t WHERE t.id = ?1",
            track_columns("t")
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| track_from_row(&row)).transpose()
    }

    pub async fn albums_for_artist(&self, artist_id: &str) -> Result<Vec<Album>> {
        let sql =
            format!("SELECT {ALBUM_COLUMNS} FROM albums WHERE artist_id = ?1 ORDER BY year, title");
        let rows = sqlx::query(&sql)
            .bind(artist_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(album_from_row).collect()
    }

    pub async fn tracks_for_artist(&self, artist_id: &str) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM tracks t WHERE t.artist_id = ?1
             ORDER BY t.year, t.album_title, t.disc_number, t.track_number, t.title",
            track_columns("t")
        );
        let rows = sqlx::query(&sql)
            .bind(artist_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(track_from_row).collect()
    }

    pub async fn tracks_for_album(&self, album_id: &str) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {} FROM tracks t WHERE t.album_id = ?1
             ORDER BY t.disc_number, t.track_number, t.title",
            track_columns("t")
        );
        let rows = sqlx::query(&sql)
            .bind(album_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(track_from_row).collect()
    }

    /// The provider id that owns an album's files (taken from any of its tracks).
    /// Used to decide how to serve the album's cover art (local disk vs. a network
    /// source like SMB). Returns `None` for an unknown/empty album.
    pub async fn album_provider_id(&self, album_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT provider_id FROM tracks WHERE album_id = ?1 LIMIT 1")
            .bind(album_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(row.try_get("provider_id")?)),
            None => Ok(None),
        }
    }

    /// Browse facets (genre/composer/year/folder), computed in SQL where practical.
    pub async fn browse_index(&self) -> Result<BrowseIndex> {
        let genres = self.text_facet("genres").await?;
        let composers = self.text_facet("composers").await?;

        let year_rows = sqlx::query(
            "SELECT year, COUNT(*) AS n FROM tracks WHERE year IS NOT NULL GROUP BY year ORDER BY year DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut years = Vec::with_capacity(year_rows.len());
        for row in year_rows {
            years.push(BrowseYearFacet {
                value: optional_i64_to_u16(row.try_get("year")?, "year")?
                    .ok_or_else(|| anyhow::anyhow!("null year in facet"))?,
                track_count: i64_to_usize(row.try_get("n")?, "track_count")?,
            });
        }

        // Folders are derived from each track's relative path; computed in Rust to
        // match the scanner's parent-directory semantics exactly.
        let path_rows = sqlx::query("SELECT relative_path FROM tracks")
            .fetch_all(&self.pool)
            .await?;
        let mut folder_counts: BTreeMap<String, usize> = BTreeMap::new();
        for row in path_rows {
            let relative_path: String = row.try_get("relative_path")?;
            if let Some((folder, _file)) = relative_path.rsplit_once('/') {
                *folder_counts.entry(folder.to_string()).or_insert(0) += 1;
            }
        }
        let folders = folder_counts
            .into_iter()
            .map(|(value, track_count)| BrowseTextFacet { value, track_count })
            .collect();

        let tags = self.tag_facet().await?;

        Ok(BrowseIndex {
            genres,
            years,
            composers,
            folders,
            tags,
        })
    }

    /// Distinct-track counts per musicata-ml audio tag, most-common first. Reads the
    /// `track_features.tags_json` array (`[{label, score}, …]`), counting a track once
    /// per distinct label it carries. Empty until tracks have been analysed.
    async fn tag_facet(&self) -> Result<Vec<BrowseTextFacet>> {
        // Alias the label as `label` (not `value` — `json_each` already exposes a `value`
        // column, and grouping on that would split identical labels by their differing score).
        let rows = sqlx::query(
            "SELECT json_extract(je.value, '$.label') AS label, COUNT(DISTINCT f.track_id) AS n
             FROM track_features f, json_each(f.tags_json) je
             WHERE json_extract(je.value, '$.label') IS NOT NULL
               AND trim(json_extract(je.value, '$.label')) <> ''
             GROUP BY label ORDER BY n DESC, label",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut facets = Vec::with_capacity(rows.len());
        for row in rows {
            facets.push(BrowseTextFacet {
                value: row.try_get("label")?,
                track_count: i64_to_usize(row.try_get("n")?, "track_count")?,
            });
        }
        Ok(facets)
    }

    /// Distinct-track counts per value of a JSON-array observation column.
    async fn text_facet(&self, column: &str) -> Result<Vec<BrowseTextFacet>> {
        let sql = format!(
            "SELECT json_each.value AS value, COUNT(DISTINCT o.track_id) AS n
             FROM track_metadata_observations o, json_each(o.{column})
             WHERE trim(json_each.value) <> ''
             GROUP BY json_each.value ORDER BY json_each.value"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut facets = Vec::with_capacity(rows.len());
        for row in rows {
            facets.push(BrowseTextFacet {
                value: row.try_get("value")?,
                track_count: i64_to_usize(row.try_get("n")?, "track_count")?,
            });
        }
        Ok(facets)
    }

    /// Update an album's selected artwork without rewriting the whole library.
    pub async fn set_album_artwork(
        &self,
        album_id: &str,
        artwork_path: Option<&Path>,
        artwork_url: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE albums SET artwork_path = ?1, artwork_url = ?2 WHERE id = ?3")
            .bind(artwork_path.map(|path| path.to_string_lossy().to_string()))
            .bind(artwork_url)
            .bind(album_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Acquired (externally fetched) album artwork -------------------------

    /// Albums that still need a cover fetched from an external provider: no
    /// folder/embedded cover (`artwork_url IS NULL`), and either never attempted or
    /// last marked `not_found` before `retry_before` (so a coverless album isn't
    /// re-queried on every rescan). Already-`acquired` albums are excluded.
    pub async fn albums_missing_artwork(
        &self,
        retry_before_unix_seconds: i64,
    ) -> Result<Vec<AlbumArtworkTarget>> {
        let rows = sqlx::query(
            "SELECT a.id, a.artist_name, a.title
             FROM albums a
             LEFT JOIN acquired_album_artwork q ON q.album_id = a.id
             WHERE a.artwork_url IS NULL
               AND (q.album_id IS NULL
                    OR (q.status = 'not_found' AND q.acquired_at_unix_seconds < ?1))
             ORDER BY a.artist_name, a.title",
        )
        .bind(retry_before_unix_seconds)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AlbumArtworkTarget {
                    album_id: row.try_get("id")?,
                    artist_name: row.try_get("artist_name")?,
                    title: row.try_get("title")?,
                })
            })
            .collect()
    }

    /// Record the outcome of an artwork fetch for an album (`acquired` with the cache
    /// reference, or `not_found`).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_acquired_artwork(
        &self,
        album_id: &str,
        provider: &str,
        remote_url: Option<&str>,
        cache_key: Option<&str>,
        ext: Option<&str>,
        width: Option<u32>,
        status: &str,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO acquired_album_artwork
                (album_id, provider, remote_url, cache_key, ext, width, status, acquired_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(album_id) DO UPDATE SET
                provider = ?2, remote_url = ?3, cache_key = ?4, ext = ?5, width = ?6,
                status = ?7, acquired_at_unix_seconds = ?8",
        )
        .bind(album_id)
        .bind(provider)
        .bind(remote_url)
        .bind(cache_key)
        .bind(ext)
        .bind(width.map(|w| w as i64))
        .bind(status)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The acquired-artwork record for an album, if any (used by the serve handler).
    pub async fn acquired_artwork(&self, album_id: &str) -> Result<Option<AcquiredArtwork>> {
        let Some(row) = sqlx::query(
            "SELECT provider, remote_url, cache_key, ext, width, status
             FROM acquired_album_artwork WHERE album_id = ?1",
        )
        .bind(album_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(AcquiredArtwork {
            provider: row.try_get("provider")?,
            remote_url: row.try_get("remote_url")?,
            cache_key: row.try_get("cache_key")?,
            ext: row.try_get("ext")?,
            width: row.try_get::<Option<i64>, _>("width")?.map(|w| w as u32),
            status: row.try_get("status")?,
        }))
    }

    /// Re-apply acquired covers' `artwork_url` to the albums table after a rescan wiped
    /// it (`save_library` rebuilds albums each scan). Pure DB, no network. Returns how
    /// many albums were restored.
    pub async fn reapply_acquired_artwork(&self) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE albums SET artwork_url =
                '/api/albums/' || id || '/artwork?asset=' ||
                (SELECT cache_key FROM acquired_album_artwork q
                 WHERE q.album_id = albums.id AND q.status = 'acquired')
             WHERE artwork_url IS NULL
               AND EXISTS (SELECT 1 FROM acquired_album_artwork q
                           WHERE q.album_id = albums.id AND q.status = 'acquired')",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // ---- Acquired (externally fetched) artist artwork ------------------------

    /// Artists that still need an image fetched: no `artwork_url`, and either never
    /// attempted or last marked `not_found` before `retry_before` (so a coverless artist
    /// isn't re-queried on every rescan). Already-`acquired` artists are excluded.
    pub async fn artists_missing_artwork(
        &self,
        retry_before_unix_seconds: i64,
    ) -> Result<Vec<ArtistArtworkTarget>> {
        let rows = sqlx::query(
            "SELECT a.id, a.name
             FROM artists a
             LEFT JOIN acquired_artist_artwork q ON q.artist_id = a.id
             WHERE a.artwork_url IS NULL
               AND (q.artist_id IS NULL
                    OR (q.status = 'not_found' AND q.acquired_at_unix_seconds < ?1))
             ORDER BY a.track_count DESC, a.name",
        )
        .bind(retry_before_unix_seconds)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(ArtistArtworkTarget {
                    artist_id: row.try_get("id")?,
                    name: row.try_get("name")?,
                })
            })
            .collect()
    }

    /// Record the outcome of an artist-image fetch (`acquired` with the cache reference,
    /// or `not_found`).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_acquired_artist_artwork(
        &self,
        artist_id: &str,
        provider: &str,
        remote_url: Option<&str>,
        cache_key: Option<&str>,
        ext: Option<&str>,
        width: Option<u32>,
        status: &str,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO acquired_artist_artwork
                (artist_id, provider, remote_url, cache_key, ext, width, status, acquired_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(artist_id) DO UPDATE SET
                provider = ?2, remote_url = ?3, cache_key = ?4, ext = ?5, width = ?6,
                status = ?7, acquired_at_unix_seconds = ?8",
        )
        .bind(artist_id)
        .bind(provider)
        .bind(remote_url)
        .bind(cache_key)
        .bind(ext)
        .bind(width.map(|w| w as i64))
        .bind(status)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The acquired-image record for an artist, if any (used by the serve handler).
    pub async fn acquired_artist_artwork(&self, artist_id: &str) -> Result<Option<AcquiredArtwork>> {
        let Some(row) = sqlx::query(
            "SELECT provider, remote_url, cache_key, ext, width, status
             FROM acquired_artist_artwork WHERE artist_id = ?1",
        )
        .bind(artist_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(AcquiredArtwork {
            provider: row.try_get("provider")?,
            remote_url: row.try_get("remote_url")?,
            cache_key: row.try_get("cache_key")?,
            ext: row.try_get("ext")?,
            width: row.try_get::<Option<i64>, _>("width")?.map(|w| w as u32),
            status: row.try_get("status")?,
        }))
    }

    /// Re-apply acquired artist images' `artwork_url` after a rescan wiped the artists
    /// table. Pure DB, no network. Returns how many artists were restored.
    pub async fn reapply_acquired_artist_artwork(&self) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE artists SET artwork_url =
                '/api/artists/' || id || '/artwork?asset=' ||
                (SELECT cache_key FROM acquired_artist_artwork q
                 WHERE q.artist_id = artists.id AND q.status = 'acquired')
             WHERE artwork_url IS NULL
               AND EXISTS (SELECT 1 FROM acquired_artist_artwork q
                           WHERE q.artist_id = artists.id AND q.status = 'acquired')",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Re-point id-referencing rows after the artist/album id derivation changed (e.g. the
    /// normalization rollout, or a merge). `artist_id`/`album_id` are derived from names,
    /// so changing the derivation orphans `favorites` and `acquired_*` rows that hold the
    /// old id (track ids are unaffected). This recomputes old→new from the current
    /// artist/album rows (name-keyed) and moves those references, collapsing duplicates
    /// (two old ids → one new merges the stars). Run it BEFORE the regroup+save that
    /// rewrites the artist/album rows with the new ids. Returns rows moved.
    pub async fn remap_identity_references(&self) -> Result<u64> {
        // Resolve names through user artist-merges so a merge moves the member's stars/art
        // onto the canonical id (matching the regroup's id derivation).
        let aliases = self.artist_alias_map().await?;
        let resolve = |name: &str| -> String {
            aliases
                .get(&musicata_core::normalize_artist_key(name))
                .cloned()
                .unwrap_or_else(|| name.to_string())
        };
        // old artist id -> new artist id (only where it actually changes).
        let artist_rows = sqlx::query("SELECT id, name FROM artists")
            .fetch_all(&self.pool)
            .await?;
        let mut artist_map: Vec<(String, String)> = Vec::new();
        for row in &artist_rows {
            let old: String = row.try_get("id")?;
            let name: String = row.try_get("name")?;
            let new = artist_identity(&resolve(&name), None);
            if new != old {
                artist_map.push((old, new));
            }
        }
        let album_rows = sqlx::query("SELECT id, artist_name, title FROM albums")
            .fetch_all(&self.pool)
            .await?;
        let mut album_map: Vec<(String, String)> = Vec::new();
        for row in &album_rows {
            let old: String = row.try_get("id")?;
            let artist_name: String = row.try_get("artist_name")?;
            let title: String = row.try_get("title")?;
            let new = album_identity(&resolve(&artist_name), &title, None);
            if new != old {
                album_map.push((old, new));
            }
        }
        if artist_map.is_empty() && album_map.is_empty() {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        let mut moved = 0u64;
        // For each old→new: move the reference if the destination is free (UPDATE OR
        // IGNORE), then delete any leftover old row (the move was blocked by a duplicate —
        // the star/cover collapses onto the existing new one).
        for (kind, map) in [("artist", &artist_map), ("album", &album_map)] {
            let acquired_table = if kind == "artist" {
                "acquired_artist_artwork"
            } else {
                "acquired_album_artwork"
            };
            let acquired_col = if kind == "artist" {
                "artist_id"
            } else {
                "album_id"
            };
            for (old, new) in map {
                let fav = sqlx::query(
                    "UPDATE OR IGNORE favorites SET item_id = ?1
                     WHERE item_type = ?2 AND item_id = ?3",
                )
                .bind(new)
                .bind(kind)
                .bind(old)
                .execute(&mut *tx)
                .await?;
                moved += fav.rows_affected();
                sqlx::query("DELETE FROM favorites WHERE item_type = ?1 AND item_id = ?2")
                    .bind(kind)
                    .bind(old)
                    .execute(&mut *tx)
                    .await?;
                let sql = format!(
                    "UPDATE OR IGNORE {acquired_table} SET {acquired_col} = ?1 WHERE {acquired_col} = ?2"
                );
                sqlx::query(&sql)
                    .bind(new)
                    .bind(old)
                    .execute(&mut *tx)
                    .await?;
                let del = format!("DELETE FROM {acquired_table} WHERE {acquired_col} = ?1");
                sqlx::query(&del).bind(old).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(moved)
    }

    // ---- Artist aliases (user-curated merges) --------------------------------

    /// Record that `alias_key` (a normalized artist name key) is the same artist as
    /// `canonical_key`/`canonical_name`. Flattens chains (if the canonical is itself an
    /// alias, resolve to the ultimate canonical) and repoints any existing aliases that
    /// pointed at `alias_key`, so the map stays single-hop. No-op for a self-alias.
    pub async fn add_artist_alias(
        &self,
        alias_key: &str,
        canonical_key: &str,
        canonical_name: &str,
        now_unix_seconds: i64,
    ) -> Result<()> {
        if alias_key == canonical_key || alias_key.is_empty() {
            return Ok(());
        }
        // Resolve the canonical through any existing alias chain.
        let mut canon_key = canonical_key.to_string();
        let mut canon_name = canonical_name.to_string();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        while let Some(row) = sqlx::query(
            "SELECT canonical_key, canonical_name FROM artist_aliases WHERE alias_key = ?1",
        )
        .bind(&canon_key)
        .fetch_optional(&self.pool)
        .await?
        {
            if !seen.insert(canon_key.clone()) {
                break; // cycle guard
            }
            canon_name = row.try_get("canonical_name")?;
            canon_key = row.try_get("canonical_key")?;
            if canon_key == alias_key {
                return Err(anyhow::anyhow!("artist alias would form a cycle"));
            }
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT OR REPLACE INTO artist_aliases
                (alias_key, canonical_key, canonical_name, created_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(alias_key)
        .bind(&canon_key)
        .bind(&canon_name)
        .bind(now_unix_seconds)
        .execute(&mut *tx)
        .await?;
        // Any alias that pointed at this newly-aliased key now points one hop further.
        sqlx::query(
            "UPDATE artist_aliases SET canonical_key = ?1, canonical_name = ?2
             WHERE canonical_key = ?3",
        )
        .bind(&canon_key)
        .bind(&canon_name)
        .bind(alias_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Remove a single alias (unmerge one member).
    pub async fn remove_artist_alias(&self, alias_key: &str) -> Result<()> {
        sqlx::query("DELETE FROM artist_aliases WHERE alias_key = ?1")
            .bind(alias_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All alias rows, ordered by canonical then member (the API groups them).
    pub async fn list_artist_aliases(&self) -> Result<Vec<ArtistAlias>> {
        let rows = sqlx::query(
            "SELECT alias_key, canonical_key, canonical_name FROM artist_aliases
             ORDER BY canonical_name, alias_key",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(ArtistAlias {
                    alias_key: row.try_get("alias_key")?,
                    canonical_key: row.try_get("canonical_key")?,
                    canonical_name: row.try_get("canonical_name")?,
                })
            })
            .collect()
    }

    /// The resolver the regroup applies: normalized alias key → canonical display name.
    pub async fn artist_alias_map(&self) -> Result<BTreeMap<String, String>> {
        let rows = sqlx::query("SELECT alias_key, canonical_name FROM artist_aliases")
            .fetch_all(&self.pool)
            .await?;
        let mut map = BTreeMap::new();
        for row in &rows {
            map.insert(row.try_get("alias_key")?, row.try_get("canonical_name")?);
        }
        Ok(map)
    }

    /// The first MusicBrainz release / release-group ids found among an album's tracks
    /// (used to query id-keyed artwork providers like Cover Art Archive). Prefers ids
    /// from the embedded tags (observations) and falls back to AcoustID fingerprint
    /// results, so a fingerprinted untagged album still reaches the id-exact providers.
    pub async fn album_musicbrainz_ids(
        &self,
        album_id: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let row = sqlx::query(
            "SELECT COALESCE(o.musicbrainz_release_id, f.musicbrainz_release_id) AS rel,
                    COALESCE(o.musicbrainz_release_group_id, f.musicbrainz_release_group_id) AS rg
             FROM tracks t
             LEFT JOIN track_metadata_observations o ON o.track_id = t.id
             LEFT JOIN track_fingerprint f
                    ON f.track_id = t.id AND f.status = 'resolved'
             WHERE t.album_id = ?1
               AND (o.musicbrainz_release_id IS NOT NULL
                    OR o.musicbrainz_release_group_id IS NOT NULL
                    OR f.musicbrainz_release_id IS NOT NULL
                    OR f.musicbrainz_release_group_id IS NOT NULL)
             LIMIT 1",
        )
        .bind(album_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(row) => (row.try_get("rel")?, row.try_get("rg")?),
            None => (None, None),
        })
    }

    // ---- AcoustID fingerprint results ----------------------------------------

    /// Tracks that still need fingerprinting: no MusicBrainz recording/release id in
    /// their tags (observations), and either never fingerprinted or last `not_found`
    /// before `retry_before` (so an unidentifiable track isn't re-fingerprinted every
    /// rescan). Returns (track_id, title, artist, extension, provider_id, item_id) so the
    /// caller can read and decode the file.
    pub async fn tracks_missing_fingerprints(
        &self,
        retry_before_unix_seconds: i64,
        limit: i64,
    ) -> Result<Vec<TrackFingerprintTarget>> {
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.artist_name, t.extension, t.provider_id,
                    t.provider_item_id, t.path, t.duration_seconds
             FROM tracks t
             LEFT JOIN track_fingerprint f ON f.track_id = t.id
             WHERE NOT EXISTS (
                       SELECT 1 FROM track_metadata_observations o
                       WHERE o.track_id = t.id
                         AND (o.musicbrainz_recording_id IS NOT NULL
                              OR o.musicbrainz_release_id IS NOT NULL
                              OR o.musicbrainz_release_group_id IS NOT NULL))
               AND (f.track_id IS NULL
                    OR (f.status = 'not_found'
                        AND f.fingerprinted_at_unix_seconds < ?1))
             ORDER BY t.artist_name, t.title
             LIMIT ?2",
        )
        .bind(retry_before_unix_seconds)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(TrackFingerprintTarget {
                    track_id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    artist_name: row.try_get("artist_name")?,
                    extension: row.try_get("extension")?,
                    provider_id: row.try_get("provider_id")?,
                    provider_item_id: row.try_get("provider_item_id")?,
                    path: row.try_get("path")?,
                    duration_seconds: row.try_get("duration_seconds")?,
                })
            })
            .collect()
    }

    /// Record a fingerprint outcome for a track (`resolved` with MBIDs, or `not_found`).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_track_fingerprint(
        &self,
        track_id: &str,
        status: &str,
        recording_mbid: Option<&str>,
        release_mbid: Option<&str>,
        release_group_mbid: Option<&str>,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO track_fingerprint
                (track_id, status, musicbrainz_recording_id, musicbrainz_release_id,
                 musicbrainz_release_group_id, fingerprinted_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(track_id) DO UPDATE SET
                status = ?2, musicbrainz_recording_id = ?3, musicbrainz_release_id = ?4,
                musicbrainz_release_group_id = ?5, fingerprinted_at_unix_seconds = ?6",
        )
        .bind(track_id)
        .bind(status)
        .bind(recording_mbid)
        .bind(release_mbid)
        .bind(release_group_mbid)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Tracks with no EBU R128 loudness analysis yet — candidates for the loudness pass.
    /// Reuses [`TrackFingerprintTarget`] (it carries exactly what's needed to read + decode
    /// the file). Anti-joins `track_loudness`, so once analyzed a track is never re-queued.
    pub async fn tracks_missing_loudness(
        &self,
        limit: i64,
    ) -> Result<Vec<TrackFingerprintTarget>> {
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.artist_name, t.extension, t.provider_id,
                    t.provider_item_id, t.path, t.duration_seconds
             FROM tracks t
             LEFT JOIN track_loudness l ON l.track_id = t.id
             WHERE l.track_id IS NULL
             ORDER BY t.artist_name, t.title
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(TrackFingerprintTarget {
                    track_id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    artist_name: row.try_get("artist_name")?,
                    extension: row.try_get("extension")?,
                    provider_id: row.try_get("provider_id")?,
                    provider_item_id: row.try_get("provider_item_id")?,
                    path: row.try_get("path")?,
                    duration_seconds: row.try_get("duration_seconds")?,
                })
            })
            .collect()
    }

    // ---- Audio embeddings (musicata-ml) ----------------------------------------

    /// Library tracks with no audio-ML analysis yet — the embedding worker's backlog.
    pub async fn tracks_missing_embedding(
        &self,
        limit: i64,
    ) -> Result<Vec<TrackFingerprintTarget>> {
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.artist_name, t.extension, t.provider_id,
                    t.provider_item_id, t.path, t.duration_seconds
             FROM tracks t
             LEFT JOIN track_features f ON f.track_id = t.id
             WHERE f.track_id IS NULL
             ORDER BY t.artist_name, t.title
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(TrackFingerprintTarget {
                    track_id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    artist_name: row.try_get("artist_name")?,
                    extension: row.try_get("extension")?,
                    provider_id: row.try_get("provider_id")?,
                    provider_item_id: row.try_get("provider_item_id")?,
                    path: row.try_get("path")?,
                    duration_seconds: row.try_get("duration_seconds")?,
                })
            })
            .collect()
    }

    /// Store a track's audio embedding (in the `vec0` index) plus its model provenance + tags.
    /// vec0 has no UPSERT, so the vector row is replaced; the feature row is upserted.
    pub async fn upsert_track_embedding(
        &self,
        track_id: &str,
        embedding: &[f32],
        model: &str,
        version: Option<&str>,
        tags_json: &str,
        now_unix_seconds: i64,
    ) -> Result<()> {
        let blob = embedding_to_blob(embedding);
        sqlx::query("DELETE FROM track_embedding WHERE track_id = ?1")
            .bind(track_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO track_embedding (track_id, embedding) VALUES (?1, ?2)")
            .bind(track_id)
            .bind(&blob[..])
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO track_features (track_id, model, version, tags_json, analyzed_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(track_id) DO UPDATE SET
                model = ?2, version = ?3, tags_json = ?4, analyzed_at_unix_seconds = ?5",
        )
        .bind(track_id)
        .bind(model)
        .bind(version)
        .bind(tags_json)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many tracks have an embedding (for `/admin` status).
    pub async fn embedding_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM track_features").fetch_one(&self.pool).await?)
    }

    /// The `k` tracks most similar to `seed_track_id` by audio embedding (KNN over the vec0
    /// index), nearest first, excluding the seed itself. Empty if the seed has no embedding.
    pub async fn similar_by_embedding(
        &self,
        seed_track_id: &str,
        k: usize,
    ) -> Result<Vec<(String, f64)>> {
        let seed: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT embedding FROM track_embedding WHERE track_id = ?1")
                .bind(seed_track_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(seed) = seed else {
            return Ok(Vec::new());
        };
        // Fetch k+1 so dropping the seed (distance 0 to itself) still leaves k.
        let rows = sqlx::query(
            "SELECT track_id, distance FROM track_embedding
             WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
        )
        .bind(&seed[..])
        .bind((k + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let id: String = row.try_get("track_id").ok()?;
                let distance: f64 = row.try_get("distance").ok()?;
                (id != seed_track_id).then_some((id, distance))
            })
            .take(k)
            .collect())
    }

    /// A diverse audio "radio" seeded from a track: the embedding nearest-neighbors, but
    /// **interleaved across artists** so the station doesn't just repeat one artist (a good DJ
    /// switches it up). Pulls a wider candidate pool from the index, then [`diversify`]s it down
    /// to `limit`. Empty if the seed has no embedding.
    pub async fn audio_radio(&self, seed_track_id: &str, limit: usize) -> Result<Vec<String>> {
        // A wide pool so the diversifier has several artists to interleave (the raw KNN is
        // artist-heavy: tracks by the same artist tend to cluster acoustically).
        let pool = self
            .similar_by_embedding(seed_track_id, limit.saturating_mul(5).max(limit))
            .await?;
        if pool.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = pool.into_iter().map(|(id, _distance)| id).collect();
        let artists = self.artist_names_for(&ids).await?;
        let ranked: Vec<(String, String)> = ids
            .into_iter()
            .map(|id| {
                let artist = artists.get(&id).cloned().unwrap_or_default();
                (id, artist)
            })
            .collect();
        Ok(diversify(&ranked, limit))
    }

    /// Map a set of track ids to their artist names (order not preserved).
    async fn artist_names_for(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT id, artist_name FROM tracks WHERE id IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| Ok((row.try_get("id")?, row.try_get("artist_name")?)))
            .collect()
    }

    /// Store a track's measured loudness (integrated LUFS + true-peak dBTP). A NULL `lufs`
    /// marks a track analyzed but un-measurable (e.g. silent / undecodable) so it isn't retried.
    pub async fn upsert_track_loudness(
        &self,
        track_id: &str,
        integrated_lufs: Option<f64>,
        true_peak_dbtp: Option<f64>,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO track_loudness
                (track_id, integrated_lufs, true_peak_dbtp, analyzed_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(track_id) DO UPDATE SET
                integrated_lufs = ?2, true_peak_dbtp = ?3, analyzed_at_unix_seconds = ?4",
        )
        .bind(track_id)
        .bind(integrated_lufs)
        .bind(true_peak_dbtp)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A track's measured (integrated LUFS, true-peak dBTP), if analyzed and measurable.
    /// Feeds the per-track playback leveling gain.
    pub async fn track_loudness(&self, track_id: &str) -> Result<Option<(f64, f64)>> {
        let row = sqlx::query(
            "SELECT integrated_lufs, true_peak_dbtp FROM track_loudness
             WHERE track_id = ?1 AND integrated_lufs IS NOT NULL",
        )
        .bind(track_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => {
                let lufs: f64 = row.try_get("integrated_lufs")?;
                let peak: Option<f64> = row.try_get("true_peak_dbtp")?;
                Ok(Some((lufs, peak.unwrap_or(0.0))))
            }
            None => Ok(None),
        }
    }

    /// An album's aggregate (integrated LUFS, true-peak dBTP), if computed. Feeds the
    /// browser's album-mode volume leveling.
    pub async fn album_loudness(&self, album_id: &str) -> Result<Option<(f64, f64)>> {
        let row = sqlx::query(
            "SELECT integrated_lufs, true_peak_dbtp FROM album_loudness
             WHERE album_id = ?1 AND integrated_lufs IS NOT NULL",
        )
        .bind(album_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => {
                let lufs: f64 = row.try_get("integrated_lufs")?;
                let peak: Option<f64> = row.try_get("true_peak_dbtp")?;
                Ok(Some((lufs, peak.unwrap_or(0.0))))
            }
            None => Ok(None),
        }
    }

    /// Recompute every album's aggregate loudness from the per-track measurements and store
    /// it in `album_loudness`. Album loudness is the **duration-weighted mean** of the tracks'
    /// integrated loudness (mean in the linear domain, converted back to LUFS — see
    /// [`album_loudness_from_tracks`]); the album true-peak is the max across its tracks.
    /// Returns how many albums were written. Cheap and only run after a loudness pass that
    /// produced new data.
    pub async fn recompute_album_loudness(&self, now_unix_seconds: i64) -> Result<usize> {
        let rows = sqlx::query(
            "SELECT t.album_id AS album_id, l.integrated_lufs AS lufs,
                    l.true_peak_dbtp AS peak, t.duration_seconds AS duration
             FROM track_loudness l JOIN tracks t ON t.id = l.track_id
             WHERE l.integrated_lufs IS NOT NULL
             ORDER BY t.album_id",
        )
        .fetch_all(&self.pool)
        .await?;

        // Fold consecutive rows per album (ordered by album_id).
        let mut written = 0;
        let mut current: Option<String> = None;
        let mut batch: Vec<(f64, Option<f64>, Option<f64>)> = Vec::new();
        for row in &rows {
            let album_id: String = row.try_get("album_id")?;
            let lufs: f64 = row.try_get("lufs")?;
            let peak: Option<f64> = row.try_get("peak")?;
            let duration: Option<f64> = row.try_get("duration")?;
            if current.as_deref() != Some(album_id.as_str()) {
                if let Some(id) = current.take() {
                    self.store_album_loudness(&id, &batch, now_unix_seconds).await?;
                    written += 1;
                }
                current = Some(album_id);
                batch.clear();
            }
            batch.push((lufs, duration, peak));
        }
        if let Some(id) = current.take() {
            self.store_album_loudness(&id, &batch, now_unix_seconds).await?;
            written += 1;
        }
        Ok(written)
    }

    async fn store_album_loudness(
        &self,
        album_id: &str,
        tracks: &[(f64, Option<f64>, Option<f64>)],
        now_unix_seconds: i64,
    ) -> Result<()> {
        let (lufs, peak) = album_loudness_from_tracks(tracks);
        sqlx::query(
            "INSERT INTO album_loudness
                (album_id, integrated_lufs, true_peak_dbtp, analyzed_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(album_id) DO UPDATE SET
                integrated_lufs = ?2, true_peak_dbtp = ?3, analyzed_at_unix_seconds = ?4",
        )
        .bind(album_id)
        .bind(lufs)
        .bind(peak)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Re-attach per-track *and* per-album loudness to queue items loaded from the persisted
    /// snapshot (the queue tables don't store it). Keeps a restored queue's volume leveling
    /// (track and album mode) working.
    async fn fill_queue_loudness(&self, items: &mut [QueueItem]) -> Result<()> {
        for item in items.iter_mut() {
            let Some(id) = item.track_id.clone() else {
                continue;
            };
            if let Some((lufs, peak)) = self.track_loudness(&id).await? {
                item.integrated_loudness_lufs = Some(lufs);
                item.true_peak_dbtp = Some(peak);
            }
            let album_id: Option<String> =
                sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?1")
                    .bind(&id)
                    .fetch_optional(&self.pool)
                    .await?;
            if let Some(album_id) = album_id
                && let Some((lufs, peak)) = self.album_loudness(&album_id).await?
            {
                item.album_integrated_loudness_lufs = Some(lufs);
                item.album_true_peak_dbtp = Some(peak);
            }
        }
        Ok(())
    }

    // ---- Recommendations (similar / radio / continuous play) -------------------

    /// Cached similarity payload for a seed MBID: (payload_json, fetched_at). `entity_kind` is
    /// e.g. "recording". An empty-array payload is a `not_found` marker.
    pub async fn get_similarity_cache(
        &self,
        entity_kind: &str,
        seed_mbid: &str,
    ) -> Result<Option<(String, i64)>> {
        let row = sqlx::query(
            "SELECT payload_json, fetched_at_unix_seconds FROM similarity_cache
             WHERE entity_kind = ?1 AND seed_mbid = ?2",
        )
        .bind(entity_kind)
        .bind(seed_mbid)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some((
                row.try_get("payload_json")?,
                row.try_get("fetched_at_unix_seconds")?,
            ))),
            None => Ok(None),
        }
    }

    pub async fn set_similarity_cache(
        &self,
        entity_kind: &str,
        seed_mbid: &str,
        payload_json: &str,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO similarity_cache (entity_kind, seed_mbid, payload_json, fetched_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(entity_kind, seed_mbid) DO UPDATE SET
                payload_json = ?3, fetched_at_unix_seconds = ?4",
        )
        .bind(entity_kind)
        .bind(seed_mbid)
        .bind(payload_json)
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A track's MusicBrainz recording id, from its tags or its AcoustID fingerprint. Used to
    /// seed ListenBrainz similar-recordings.
    pub async fn track_recording_mbid(&self, track_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT COALESCE(
                (SELECT o.musicbrainz_recording_id FROM track_metadata_observations o
                   WHERE o.track_id = ?1 AND o.musicbrainz_recording_id IS NOT NULL LIMIT 1),
                (SELECT f.musicbrainz_recording_id FROM track_fingerprint f
                   WHERE f.track_id = ?1 AND f.musicbrainz_recording_id IS NOT NULL LIMIT 1)
             ) AS mbid",
        )
        .bind(track_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| row.try_get::<Option<String>, _>("mbid").ok().flatten()))
    }

    /// Resolve external recording MBIDs to local track ids (first local track per MBID, in the
    /// input order). The lingua franca for turning ListenBrainz results into playable tracks.
    pub async fn tracks_for_recording_mbids(&self, mbids: &[String]) -> Result<Vec<String>> {
        if mbids.is_empty() {
            return Ok(Vec::new());
        }
        // SQLite VALUES preserve order; map each input MBID to its first matching local track.
        let values = (1..=mbids.len())
            .map(|i| format!("(?{i})"))
            .collect::<Vec<_>>()
            .join(",");
        // A local track's recording MBID can live in any of three places: file tags
        // (observations), AcoustID fingerprinting, or MusicBrainz enrichment. Match against all
        // three so LB results resolve even for libraries whose tags carry no MBIDs.
        let query = format!(
            "WITH seeds(mbid) AS (VALUES {values}),
                  recording_ids(track_id, mbid) AS (
                      SELECT track_id, musicbrainz_recording_id FROM track_metadata_observations
                        WHERE musicbrainz_recording_id IS NOT NULL
                      UNION
                      SELECT track_id, musicbrainz_recording_id FROM track_fingerprint
                        WHERE musicbrainz_recording_id IS NOT NULL
                      UNION
                      SELECT track_id, musicbrainz_recording_id FROM track_musicbrainz_metadata
                        WHERE musicbrainz_recording_id IS NOT NULL
                  )
             SELECT s.mbid AS mbid,
                    (SELECT r.track_id FROM recording_ids r
                     JOIN tracks t ON t.id = r.track_id
                     WHERE r.mbid = s.mbid LIMIT 1) AS track_id
             FROM seeds s",
        );
        let mut q = sqlx::query(&query);
        for mbid in mbids {
            q = q.bind(mbid);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .filter_map(|row| row.try_get::<Option<String>, _>("track_id").ok().flatten())
            .collect())
    }

    /// A track's MusicBrainz artist id. Seeds ListenBrainz similar-artists, which has far better
    /// coverage than similar-recordings. Prefers file tags, falling back to MusicBrainz
    /// enrichment (`track_musicbrainz_metadata`) — the common case for libraries whose tags carry
    /// no MBIDs but which fingerprinting/enrichment has since identified.
    pub async fn track_artist_mbid(&self, track_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT COALESCE(
                (SELECT o.musicbrainz_artist_id FROM track_metadata_observations o
                   WHERE o.track_id = ?1 AND o.musicbrainz_artist_id IS NOT NULL LIMIT 1),
                (SELECT m.musicbrainz_artist_id FROM track_musicbrainz_metadata m
                   WHERE m.track_id = ?1 AND m.musicbrainz_artist_id IS NOT NULL LIMIT 1)
             ) AS mbid",
        )
        .bind(track_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| row.try_get::<Option<String>, _>("mbid").ok().flatten()))
    }

    /// Local tracks for a set of artist names (case-insensitive), as `(lowercased name, track
    /// id)` pairs in random per-artist order. Used to turn ListenBrainz similar-artists into a
    /// playable "artists like this" radio; the caller groups by name to cap tracks per artist.
    pub async fn tracks_for_artist_names(&self, names: &[String]) -> Result<Vec<(String, String)>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let lowered: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        let placeholders = (1..=lowered.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT lower(t.artist_name) AS aname, t.id AS id
             FROM tracks t
             WHERE lower(t.artist_name) IN ({placeholders})
             ORDER BY RANDOM()",
        );
        let mut q = sqlx::query(&query);
        for name in &lowered {
            q = q.bind(name);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| Ok((row.try_get("aname")?, row.try_get("id")?)))
            .collect()
    }

    /// Track ids played (event_kind='played') at or after `since_unix` — the recency-exclusion
    /// set for continuous play (don't re-queue something just heard).
    pub async fn recently_played_track_ids(&self, since_unix_seconds: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT track_id FROM listens
             WHERE listened_at_unix_seconds >= ?1 AND event_kind = 'played'",
        )
        .bind(since_unix_seconds)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("track_id")?)).collect()
    }

    /// Track ids the listener skips more than they finish — the "skip penalty" set radio and
    /// autoplay deprioritize. A track qualifies when its skips strictly exceed its confirmed
    /// plays *and* there are at least two skips (a minimum sample, so one stray skip never
    /// sidelines a track). `played`/`skipped` are the only `event_kind`s today.
    pub async fn frequently_skipped_track_ids(&self) -> Result<std::collections::HashSet<String>> {
        let rows = sqlx::query(
            "SELECT track_id
             FROM listens
             GROUP BY track_id
             HAVING SUM(CASE WHEN event_kind = 'skipped' THEN 1 ELSE 0 END)
                  > SUM(CASE WHEN event_kind = 'played' THEN 1 ELSE 0 END)
                AND SUM(CASE WHEN event_kind = 'skipped' THEN 1 ELSE 0 END) >= 2",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("track_id")?)).collect()
    }

    /// Local content similarity fallback (no network): tracks sharing a genre with the seed or
    /// by the same artist, Jellyfin-style weighted (genre 10, same-artist 30), seed excluded.
    pub async fn similar_local_track_ids(
        &self,
        seed_track_id: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "WITH seed_genres(g) AS (
                 SELECT DISTINCT json_each.value
                 FROM track_metadata_observations o, json_each(o.genres)
                 WHERE o.track_id = ?1
             ),
             seed AS (SELECT artist_id FROM tracks WHERE id = ?1)
             SELECT t.id AS id,
                 (SELECT COUNT(DISTINCT json_each.value)
                  FROM track_metadata_observations o2, json_each(o2.genres)
                  WHERE o2.track_id = t.id AND json_each.value IN (SELECT g FROM seed_genres)) * 10
                 + (CASE WHEN t.artist_id = (SELECT artist_id FROM seed) THEN 30 ELSE 0 END) AS score
             FROM tracks t
             WHERE t.id != ?1
               AND ( t.artist_id = (SELECT artist_id FROM seed)
                     OR EXISTS (SELECT 1 FROM track_metadata_observations o2, json_each(o2.genres)
                                WHERE o2.track_id = t.id
                                  AND json_each.value IN (SELECT g FROM seed_genres)) )
             ORDER BY score DESC, RANDOM()
             LIMIT ?2",
        )
        .bind(seed_track_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("id")?)).collect()
    }

    /// Drop `not_found` fingerprint markers so those tracks are re-fingerprinted on the
    /// next pass (instead of waiting out the retry window). Used once after a fix that
    /// changes the lookup (e.g. reporting the real track duration to AcoustID). Returns
    /// how many markers were cleared.
    pub async fn clear_not_found_fingerprints(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM track_fingerprint WHERE status = 'not_found'")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Tracks AcoustID couldn't identify (`track_fingerprint.status = 'not_found'`) that
    /// still carry no MusicBrainz id — candidates for the text-search fallback. Once
    /// searched, the row becomes `resolved` (a match) or `search_not_found` (no confident
    /// match), so each track is searched at most once.
    pub async fn tracks_for_musicbrainz_search(
        &self,
        limit: i64,
    ) -> Result<Vec<TrackSearchTarget>> {
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.artist_name, t.album_title, t.year
             FROM tracks t
             JOIN track_fingerprint f ON f.track_id = t.id AND f.status = 'not_found'
             WHERE NOT EXISTS (
                       SELECT 1 FROM track_metadata_observations o
                       WHERE o.track_id = t.id
                         AND (o.musicbrainz_recording_id IS NOT NULL
                              OR o.musicbrainz_release_id IS NOT NULL
                              OR o.musicbrainz_release_group_id IS NOT NULL))
             ORDER BY t.artist_name, t.title
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(TrackSearchTarget {
                    track_id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    artist_name: row.try_get("artist_name")?,
                    album_title: row.try_get("album_title")?,
                    year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                })
            })
            .collect()
    }

    // ---- Identification stats (MusicBrainz coverage) -------------------------

    async fn scalar_i64(&self, sql: &str) -> Result<i64> {
        Ok(sqlx::query(sql).fetch_one(&self.pool).await?.try_get(0)?)
    }

    /// How much of the library has been identified (resolved a MusicBrainz recording id,
    /// via embedded tags or fingerprint/search), plus the fingerprint status breakdown.
    /// Write a consistent, compacted snapshot of the whole database to `dest` (a fresh
    /// SQLite file) for library export. `VACUUM INTO` captures a stable view even while
    /// writes continue, so it's safe on the live database. `dest` must not already exist.
    pub async fn snapshot_to(&self, dest: &Path) -> Result<()> {
        // VACUUM INTO takes a string literal, not a bound parameter; escape any quotes.
        let escaped = dest.to_string_lossy().replace('\'', "''");
        sqlx::query(&format!("VACUUM INTO '{escaped}'"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn identification_stats(&self) -> Result<IdentificationStats> {
        let identified = track_identified_clause("t.id");
        Ok(IdentificationStats {
            tracks_total: self.scalar_i64("SELECT COUNT(*) FROM tracks").await?,
            tracks_identified: self
                .scalar_i64(&format!("SELECT COUNT(*) FROM tracks t WHERE {identified}"))
                .await?,
            albums_total: self.scalar_i64("SELECT COUNT(*) FROM albums").await?,
            albums_identified: self
                .scalar_i64(&format!(
                    "SELECT COUNT(*) FROM albums a WHERE EXISTS
                     (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND {identified})"
                ))
                .await?,
            artists_total: self.scalar_i64("SELECT COUNT(*) FROM artists").await?,
            artists_identified: self
                .scalar_i64(&format!(
                    "SELECT COUNT(*) FROM artists ar WHERE EXISTS
                     (SELECT 1 FROM tracks t WHERE t.artist_id = ar.id AND {identified})"
                ))
                .await?,
            tracks_processed: self
                .scalar_i64("SELECT COUNT(*) FROM track_fingerprint")
                .await?,
            fingerprint_resolved: self
                .scalar_i64("SELECT COUNT(*) FROM track_fingerprint WHERE status = 'resolved'")
                .await?,
            fingerprint_not_found: self
                .scalar_i64("SELECT COUNT(*) FROM track_fingerprint WHERE status = 'not_found'")
                .await?,
            fingerprint_searched: self
                .scalar_i64(
                    "SELECT COUNT(*) FROM track_fingerprint WHERE status = 'search_not_found'",
                )
                .await?,
            tracks_enriched: self
                .scalar_i64(
                    "SELECT COUNT(*) FROM track_musicbrainz_metadata WHERE status = 'resolved'",
                )
                .await?,
            album_covers: self
                .scalar_i64("SELECT COUNT(*) FROM albums WHERE artwork_url IS NOT NULL")
                .await?,
            artist_images: self
                .scalar_i64("SELECT COUNT(*) FROM artists WHERE artwork_url IS NOT NULL")
                .await?,
        })
    }

    /// Albums none of whose tracks carry a MusicBrainz recording id, most-tracks first —
    /// the biggest unidentified targets (no id-exact artwork, no enrichment).
    pub async fn unidentified_albums(&self, limit: i64) -> Result<Vec<Album>> {
        let identified = track_identified_clause("t.id");
        let sql = format!(
            "SELECT {ALBUM_COLUMNS} FROM albums a
             WHERE NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND {identified})
             ORDER BY track_count DESC, title LIMIT ?1"
        );
        let rows = sqlx::query(&sql).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(album_from_row).collect()
    }

    /// Artists none of whose tracks carry a MusicBrainz recording id, most-tracks first.
    pub async fn unidentified_artists(&self, limit: i64) -> Result<Vec<Artist>> {
        let identified = track_identified_clause("t.id");
        let sql = format!(
            "SELECT id, name, album_count, track_count, artwork_url FROM artists ar
             WHERE NOT EXISTS (SELECT 1 FROM tracks t WHERE t.artist_id = ar.id AND {identified})
             ORDER BY track_count DESC, name LIMIT ?1"
        );
        let rows = sqlx::query(&sql).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(artist_from_row).collect()
    }

    // ---- MusicBrainz metadata enrichment (from fingerprinted recording MBIDs) -

    /// Tracks that have a **resolved** fingerprint (a recording MBID) but no MusicBrainz
    /// metadata fetched yet — or whose last fetch was `not_found` before `retry_before`
    /// (so an unresolvable recording isn't re-fetched every rescan). The caller fetches
    /// the recording/release from MusicBrainz.
    pub async fn tracks_missing_musicbrainz_metadata(
        &self,
        retry_before_unix_seconds: i64,
        limit: i64,
    ) -> Result<Vec<MusicBrainzEnrichTarget>> {
        let rows = sqlx::query(
            "SELECT f.track_id, f.musicbrainz_recording_id, f.musicbrainz_release_id,
                    f.musicbrainz_release_group_id
             FROM track_fingerprint f
             LEFT JOIN track_musicbrainz_metadata m ON m.track_id = f.track_id
             WHERE f.status = 'resolved'
               AND f.musicbrainz_recording_id IS NOT NULL
               AND (m.track_id IS NULL
                    OR (m.status = 'not_found'
                        AND m.enriched_at_unix_seconds < ?1))
             LIMIT ?2",
        )
        .bind(retry_before_unix_seconds)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(MusicBrainzEnrichTarget {
                    track_id: row.try_get("track_id")?,
                    recording_mbid: row.try_get("musicbrainz_recording_id")?,
                    release_mbid: row.try_get("musicbrainz_release_id")?,
                    release_group_mbid: row.try_get("musicbrainz_release_group_id")?,
                })
            })
            .collect()
    }

    /// Record a MusicBrainz fetch outcome for a track (`resolved` with values, or
    /// `not_found`). The values are applied to the library by
    /// [`Database::reapply_canonical_grouping`].
    pub async fn upsert_track_musicbrainz_metadata(
        &self,
        track_id: &str,
        status: &str,
        meta: &ResolvedMusicBrainzMetadata,
        now_unix_seconds: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO track_musicbrainz_metadata
                (track_id, status, title, artist_name, album_title, album_artist_name,
                 track_number, track_total, disc_number, recording_date, year,
                 musicbrainz_recording_id, musicbrainz_release_id,
                 musicbrainz_release_group_id, musicbrainz_artist_id, enriched_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(track_id) DO UPDATE SET
                status = ?2, title = ?3, artist_name = ?4, album_title = ?5,
                album_artist_name = ?6, track_number = ?7, track_total = ?8,
                disc_number = ?9, recording_date = ?10, year = ?11,
                musicbrainz_recording_id = ?12, musicbrainz_release_id = ?13,
                musicbrainz_release_group_id = ?14, musicbrainz_artist_id = ?15,
                enriched_at_unix_seconds = ?16",
        )
        .bind(track_id)
        .bind(status)
        .bind(meta.title.as_deref())
        .bind(meta.artist_name.as_deref())
        .bind(meta.album_title.as_deref())
        .bind(meta.album_artist_name.as_deref())
        .bind(meta.track_number)
        .bind(meta.track_total)
        .bind(meta.disc_number)
        .bind(meta.recording_date.as_deref())
        .bind(meta.year)
        .bind(meta.recording_mbid.as_deref())
        .bind(meta.release_mbid.as_deref())
        .bind(meta.release_group_mbid.as_deref())
        .bind(meta.artist_mbid.as_deref())
        .bind(now_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Re-derive the canonical library grouping, applying two server-side layers the pure
    /// scanner can't: resolved **MusicBrainz metadata** (never clobbering an embedded tag —
    /// a field is overridden only when the file carries no `embedded_tag` observation for
    /// it) and user-curated **artist merges** (alias → canonical). Re-derives the
    /// artist/album entities so the denormalized track columns and the entity tables stay
    /// consistent. Returns the number of tracks that received MusicBrainz values.
    ///
    /// Must run after every `save_library` (the scan rewrites tracks back to folder-derived
    /// grouping) and after a merge/unmerge — same discipline as
    /// [`Database::reapply_acquired_artwork`]. Cheap no-op when there's neither enrichment
    /// nor any alias.
    pub async fn reapply_canonical_grouping(&self) -> Result<u64> {
        let resolved = sqlx::query(
            "SELECT track_id, title, artist_name, album_title, album_artist_name,
                    track_number, disc_number, year
             FROM track_musicbrainz_metadata WHERE status = 'resolved'",
        )
        .fetch_all(&self.pool)
        .await?;
        let aliases = self.artist_alias_map().await?;
        if resolved.is_empty() && aliases.is_empty() {
            return Ok(0);
        }

        // Fields the file already owns via embedded tags — never overwrite these.
        let embedded_rows = sqlx::query(
            "SELECT track_id, field_name FROM track_metadata_field_observations
             WHERE source = 'embedded_tag'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut embedded: BTreeSet<(String, String)> = BTreeSet::new();
        for row in &embedded_rows {
            embedded.insert((row.try_get("track_id")?, row.try_get("field_name")?));
        }

        let mut overrides: BTreeMap<String, TrackCanonicalOverride> = BTreeMap::new();
        for row in &resolved {
            let track_id: String = row.try_get("track_id")?;
            let allow = |field: &str| !embedded.contains(&(track_id.clone(), field.to_string()));
            let title: Option<String> = row.try_get("title")?;
            let artist_name: Option<String> = row.try_get("artist_name")?;
            let album_title: Option<String> = row.try_get("album_title")?;
            let album_artist_name: Option<String> = row.try_get("album_artist_name")?;
            let track_number =
                optional_i64_to_u16(row.try_get("track_number")?, "track_number")?;
            let disc_number = optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?;
            let year = optional_i64_to_u16(row.try_get("year")?, "year")?;
            let over = TrackCanonicalOverride {
                title: title.filter(|_| allow("title")),
                artist_name: artist_name.filter(|_| allow("artist_name")),
                album_title: album_title.filter(|_| allow("album_title")),
                // Album artist regroups the album; gate on the album_artist_name field.
                album_artist_name: album_artist_name.filter(|_| allow("album_artist_name")),
                year: year.filter(|_| allow("year")),
                track_number: track_number.filter(|_| allow("track_number")),
                disc_number: disc_number.filter(|_| allow("disc_number")),
            };
            if !over.is_empty() {
                overrides.insert(track_id, over);
            }
        }
        if overrides.is_empty() && aliases.is_empty() {
            return Ok(0);
        }

        let Some(library) = self.load_library().await? else {
            return Ok(0);
        };
        let mut regrouped = regroup_library_with_overrides(library, &overrides, &aliases);
        let applied = overrides.len() as u64;
        self.save_library(&mut regrouped).await?;
        Ok(applied)
    }

    // ---- App settings (user-editable, persisted; edited in the web UI) -------

    /// A stored setting value, or `None` if unset.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(row) => Some(row.try_get("value")?),
            None => None,
        })
    }

    /// Store (upsert) a setting value.
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Users + sessions (hashing/token generation live in the server crate) ----

    /// How many accounts exist — zero means the app is in first-run "setup" mode.
    pub async fn count_users(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("n")?)
    }

    pub async fn create_user(&self, user: &UserRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, role, api_token, created_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&user.id)
        .bind(&user.username)
        .bind(&user.password_hash)
        .bind(&user.role)
        .bind(&user.api_token)
        .bind(user.created_at_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user_by_username(&self, username: &str) -> Result<Option<UserRecord>> {
        self.user_where("username = ?1", username).await
    }

    pub async fn user_by_id(&self, id: &str) -> Result<Option<UserRecord>> {
        self.user_where("id = ?1", id).await
    }

    /// Look up the user owning an API token (Subsonic / bearer auth). Cleartext compare.
    pub async fn user_by_api_token(&self, token: &str) -> Result<Option<UserRecord>> {
        self.user_where("api_token = ?1", token).await
    }

    async fn user_where(&self, predicate: &str, value: &str) -> Result<Option<UserRecord>> {
        let row = sqlx::query(&format!(
            "SELECT id, username, password_hash, role, api_token, created_at_unix_seconds
             FROM users WHERE {predicate}"
        ))
        .bind(value)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| user_from_row(&row)).transpose()
    }

    pub async fn list_users(&self) -> Result<Vec<UserRecord>> {
        let rows = sqlx::query(
            "SELECT id, username, password_hash, role, api_token, created_at_unix_seconds
             FROM users ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(user_from_row).collect()
    }

    pub async fn delete_user(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_user_password(&self, id: &str, password_hash: &str) -> Result<()> {
        sqlx::query("UPDATE users SET password_hash = ?2 WHERE id = ?1")
            .bind(id)
            .bind(password_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_user_role(&self, id: &str, role: &str) -> Result<()> {
        sqlx::query("UPDATE users SET role = ?2 WHERE id = ?1")
            .bind(id)
            .bind(role)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_user_api_token(&self, id: &str, api_token: &str) -> Result<()> {
        sqlx::query("UPDATE users SET api_token = ?2 WHERE id = ?1")
            .bind(id)
            .bind(api_token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record a login session keyed by the **hash** of the cookie token.
    pub async fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        created_at: i64,
        expires_at: i64,
        user_agent: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at_unix_seconds, expires_at_unix_seconds, user_agent)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(created_at)
        .bind(expires_at)
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The user behind a (hashed) session token, if the session exists and hasn't expired.
    pub async fn session_user(&self, token_hash: &str, now: i64) -> Result<Option<UserRecord>> {
        let row = sqlx::query(
            "SELECT u.id, u.username, u.password_hash, u.role, u.api_token, u.created_at_unix_seconds
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at_unix_seconds > ?2",
        )
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| user_from_row(&row)).transpose()
    }

    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drop expired sessions (called periodically); returns how many were removed.
    pub async fn prune_expired_sessions(&self, now: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at_unix_seconds <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // ---- Players & zones -----------------------------------------------------

    pub async fn list_players(&self) -> Result<Vec<PlayerRecord>> {
        let rows =
            sqlx::query("SELECT id, kind, address, name, zone_id FROM players ORDER BY name, id")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(player_record_from_row).collect()
    }

    pub async fn player_record(&self, id: &str) -> Result<Option<PlayerRecord>> {
        let row = sqlx::query("SELECT id, kind, address, name, zone_id FROM players WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(player_record_from_row).transpose()
    }

    pub async fn players_in_zone(&self, zone_id: &str) -> Result<Vec<PlayerRecord>> {
        let rows =
            sqlx::query("SELECT id, kind, address, name, zone_id FROM players WHERE zone_id = ?1")
                .bind(zone_id)
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(player_record_from_row).collect()
    }

    /// Insert or replace a registered player (keyed by id derived from kind+address).
    pub async fn upsert_player(&self, record: &PlayerRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO players (id, kind, address, name, zone_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET kind = ?2, address = ?3, name = ?4, zone_id = ?5",
        )
        .bind(&record.id)
        .bind(&record.kind)
        .bind(&record.address)
        .bind(&record.name)
        .bind(&record.zone_id)
        .bind(now_unix_seconds())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Store the sha256 of a player's endpoint auth token (M10). Set once at registration;
    /// `upsert_player` leaves this column alone, so a re-register keeps the token.
    pub async fn set_player_auth_token_hash(&self, player_id: &str, token_hash: &str) -> Result<()> {
        sqlx::query("UPDATE players SET auth_token_hash = ?2 WHERE id = ?1")
            .bind(player_id)
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether any player has this exact endpoint-token hash. Used to let an authenticated
    /// endpoint fetch the audio streams it needs to play (it presents its player token; the
    /// token isn't tied to a track, so any valid endpoint token authorizes a stream — LAN-first).
    pub async fn player_token_exists(&self, token_hash: &str) -> Result<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM players WHERE auth_token_hash = ?1 LIMIT 1")
                .bind(token_hash)
                .fetch_optional(&self.pool)
                .await?;
        Ok(found.is_some())
    }

    /// The stored sha256 of a player's endpoint auth token, or `None` if it has none (the
    /// common case — server-initiated players carry no token).
    pub async fn player_auth_token_hash(&self, player_id: &str) -> Result<Option<String>> {
        let hash: Option<String> =
            sqlx::query_scalar::<_, Option<String>>("SELECT auth_token_hash FROM players WHERE id = ?1")
                .bind(player_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(hash)
    }

    pub async fn update_player_name(&self, id: &str, name: &str) -> Result<()> {
        sqlx::query("UPDATE players SET name = ?2 WHERE id = ?1")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_player_zone(&self, id: &str, zone_id: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE players SET zone_id = ?2 WHERE id = ?1")
            .bind(id)
            .bind(zone_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_player(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM player_queue_items WHERE player_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM player_queue WHERE player_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM players WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Server-owned playback queue (persisted, one per player) -------------

    /// Persist only the lightweight playback row (status/position/elapsed/volume/
    /// repeat/shuffle) without touching the queue items. Cheap enough to call on
    /// every playback change and (throttled) on position updates.
    pub async fn save_player_playback(
        &self,
        player_id: &str,
        playback: &PlayerPlayback,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO player_queue
                (player_id, status, position, elapsed_seconds, volume, repeat, shuffle, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(player_id) DO UPDATE SET
                status = ?2, position = ?3, elapsed_seconds = ?4, volume = ?5,
                repeat = ?6, shuffle = ?7, updated_at = ?8",
        )
        .bind(player_id)
        .bind(playback_status_str(playback.status))
        .bind(playback.position.map(|p| p as i64))
        .bind(playback.elapsed_seconds)
        .bind(playback.volume.map(|v| v as i64))
        .bind(repeat_mode_str(playback.repeat))
        .bind(playback.shuffle as i64)
        .bind(now_unix_seconds())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace the player's queue items (in order) and the playback row in one
    /// transaction. Use when the queue itself changes; otherwise prefer the
    /// cheaper [`save_player_playback`].
    pub async fn save_player_queue(
        &self,
        player_id: &str,
        playback: &PlayerPlayback,
        items: &[QueueItem],
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result = async {
            sqlx::query("DELETE FROM player_queue_items WHERE player_id = ?1")
                .bind(player_id)
                .execute(&mut *conn)
                .await?;
            for (seq, item) in items.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO player_queue_items
                        (player_id, seq, track_id, title, artist, album, stream_url, artwork_url)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .bind(player_id)
                .bind(seq as i64)
                .bind(&item.track_id)
                .bind(&item.title)
                .bind(&item.artist)
                .bind(&item.album)
                .bind(&item.stream_url)
                .bind(&item.artwork_url)
                .execute(&mut *conn)
                .await?;
            }
            sqlx::query(
                "INSERT INTO player_queue
                    (player_id, status, position, elapsed_seconds, volume, repeat, shuffle, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(player_id) DO UPDATE SET
                    status = ?2, position = ?3, elapsed_seconds = ?4, volume = ?5,
                    repeat = ?6, shuffle = ?7, updated_at = ?8",
            )
            .bind(player_id)
            .bind(playback_status_str(playback.status))
            .bind(playback.position.map(|p| p as i64))
            .bind(playback.elapsed_seconds)
            .bind(playback.volume.map(|v| v as i64))
            .bind(repeat_mode_str(playback.repeat))
            .bind(playback.shuffle as i64)
            .bind(now_unix_seconds())
            .execute(&mut *conn)
            .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    /// Load a player's persisted queue, or `None` if it has never had one saved.
    pub async fn load_player_queue(&self, player_id: &str) -> Result<Option<PlayerQueueSnapshot>> {
        let Some(row) = sqlx::query(
            "SELECT status, position, elapsed_seconds, volume, repeat, shuffle
             FROM player_queue WHERE player_id = ?1",
        )
        .bind(player_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let playback = PlayerPlayback {
            status: playback_status_from_str(&row.try_get::<String, _>("status")?),
            position: row
                .try_get::<Option<i64>, _>("position")?
                .map(|p| p as usize),
            elapsed_seconds: row.try_get("elapsed_seconds")?,
            volume: row.try_get::<Option<i64>, _>("volume")?.map(|v| v as u8),
            repeat: repeat_mode_from_str(&row.try_get::<String, _>("repeat")?),
            shuffle: row.try_get::<i64, _>("shuffle")? != 0,
        };
        let item_rows = sqlx::query(
            "SELECT track_id, title, artist, album, stream_url, artwork_url
             FROM player_queue_items WHERE player_id = ?1 ORDER BY seq",
        )
        .bind(player_id)
        .fetch_all(&self.pool)
        .await?;
        let mut items = item_rows
            .iter()
            .map(|row| {
                Ok(QueueItem {
                    track_id: row.try_get("track_id")?,
                    title: row.try_get("title")?,
                    artist: row.try_get("artist")?,
                    album: row.try_get("album")?,
                    stream_url: row.try_get("stream_url")?,
                    artwork_url: row.try_get("artwork_url")?,
                    ..Default::default()
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.fill_queue_loudness(&mut items).await?;
        Ok(Some(PlayerQueueSnapshot { playback, items }))
    }

    // ---- Server-owned playback queue (persisted, one per zone) ---------------
    // A zone owns a canonical queue just like a player; these mirror the
    // `save_player_*`/`load_player_queue` trio against the `zone_queue` tables.

    /// Persist only the lightweight playback row for a zone (no queue items).
    pub async fn save_zone_playback(&self, zone_id: &str, playback: &PlayerPlayback) -> Result<()> {
        sqlx::query(
            "INSERT INTO zone_queue
                (zone_id, status, position, elapsed_seconds, volume, repeat, shuffle, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(zone_id) DO UPDATE SET
                status = ?2, position = ?3, elapsed_seconds = ?4, volume = ?5,
                repeat = ?6, shuffle = ?7, updated_at = ?8",
        )
        .bind(zone_id)
        .bind(playback_status_str(playback.status))
        .bind(playback.position.map(|p| p as i64))
        .bind(playback.elapsed_seconds)
        .bind(playback.volume.map(|v| v as i64))
        .bind(repeat_mode_str(playback.repeat))
        .bind(playback.shuffle as i64)
        .bind(now_unix_seconds())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace a zone's queue items (in order) and its playback row in one
    /// transaction. Use when the queue itself changes; otherwise prefer the
    /// cheaper [`save_zone_playback`].
    pub async fn save_zone_queue(
        &self,
        zone_id: &str,
        playback: &PlayerPlayback,
        items: &[QueueItem],
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result = async {
            sqlx::query("DELETE FROM zone_queue_items WHERE zone_id = ?1")
                .bind(zone_id)
                .execute(&mut *conn)
                .await?;
            for (seq, item) in items.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO zone_queue_items
                        (zone_id, seq, track_id, title, artist, album, stream_url, artwork_url)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .bind(zone_id)
                .bind(seq as i64)
                .bind(&item.track_id)
                .bind(&item.title)
                .bind(&item.artist)
                .bind(&item.album)
                .bind(&item.stream_url)
                .bind(&item.artwork_url)
                .execute(&mut *conn)
                .await?;
            }
            sqlx::query(
                "INSERT INTO zone_queue
                    (zone_id, status, position, elapsed_seconds, volume, repeat, shuffle, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(zone_id) DO UPDATE SET
                    status = ?2, position = ?3, elapsed_seconds = ?4, volume = ?5,
                    repeat = ?6, shuffle = ?7, updated_at = ?8",
            )
            .bind(zone_id)
            .bind(playback_status_str(playback.status))
            .bind(playback.position.map(|p| p as i64))
            .bind(playback.elapsed_seconds)
            .bind(playback.volume.map(|v| v as i64))
            .bind(repeat_mode_str(playback.repeat))
            .bind(playback.shuffle as i64)
            .bind(now_unix_seconds())
            .execute(&mut *conn)
            .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    /// Load a zone's persisted queue, or `None` if it has never had one saved.
    pub async fn load_zone_queue(&self, zone_id: &str) -> Result<Option<PlayerQueueSnapshot>> {
        let Some(row) = sqlx::query(
            "SELECT status, position, elapsed_seconds, volume, repeat, shuffle
             FROM zone_queue WHERE zone_id = ?1",
        )
        .bind(zone_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let playback = PlayerPlayback {
            status: playback_status_from_str(&row.try_get::<String, _>("status")?),
            position: row
                .try_get::<Option<i64>, _>("position")?
                .map(|p| p as usize),
            elapsed_seconds: row.try_get("elapsed_seconds")?,
            volume: row.try_get::<Option<i64>, _>("volume")?.map(|v| v as u8),
            repeat: repeat_mode_from_str(&row.try_get::<String, _>("repeat")?),
            shuffle: row.try_get::<i64, _>("shuffle")? != 0,
        };
        let item_rows = sqlx::query(
            "SELECT track_id, title, artist, album, stream_url, artwork_url
             FROM zone_queue_items WHERE zone_id = ?1 ORDER BY seq",
        )
        .bind(zone_id)
        .fetch_all(&self.pool)
        .await?;
        let mut items = item_rows
            .iter()
            .map(|row| {
                Ok(QueueItem {
                    track_id: row.try_get("track_id")?,
                    title: row.try_get("title")?,
                    artist: row.try_get("artist")?,
                    album: row.try_get("album")?,
                    stream_url: row.try_get("stream_url")?,
                    artwork_url: row.try_get("artwork_url")?,
                    ..Default::default()
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.fill_queue_loudness(&mut items).await?;
        Ok(Some(PlayerQueueSnapshot { playback, items }))
    }

    pub async fn list_zones(&self) -> Result<Vec<Zone>> {
        let rows = sqlx::query("SELECT id, name FROM zones ORDER BY name, id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(Zone {
                    id: row.try_get("id")?,
                    name: row.try_get("name")?,
                })
            })
            .collect()
    }

    pub async fn insert_zone(&self, id: &str, name: &str) -> Result<()> {
        sqlx::query("INSERT INTO zones (id, name, created_at) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(name)
            .bind(now_unix_seconds())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_zone_name(&self, id: &str, name: &str) -> Result<()> {
        sqlx::query("UPDATE zones SET name = ?2 WHERE id = ?1")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_zone(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM zone_queue_items WHERE zone_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM zone_queue WHERE zone_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        // `players.zone_id` FK is ON DELETE SET NULL, so members are un-zoned.
        sqlx::query("DELETE FROM zones WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records one playback event. `listened_at` is when the track started playing;
    /// `kind` distinguishes a confirmed listen ([`ListenKind::Played`]) from a skip
    /// ([`ListenKind::Skipped`]).
    pub async fn record_listen(
        &self,
        track_id: &str,
        player_id: &str,
        listened_at: i64,
        kind: ListenKind,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO listens (track_id, player_id, listened_at_unix_seconds, event_kind)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(track_id)
        .bind(player_id)
        .bind(listened_at)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Distinct tracks paired with their most recent listen time (Unix seconds),
    /// newest first. Listens for tracks no longer in the library are skipped by the
    /// join. This is a cheap indexed query, safe to call often.
    pub async fn recently_played(&self, limit: usize) -> Result<Vec<(Track, i64)>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns}, MAX(l.listened_at_unix_seconds) AS last_listen
             FROM listens l JOIN tracks t ON t.id = l.track_id
             WHERE l.event_kind = 'played'
             GROUP BY l.track_id
             ORDER BY last_listen DESC
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| Ok((track_from_row(row)?, row.try_get("last_listen")?)))
            .collect()
    }

    /// Deletes listens older than `cutoff_unix` (Unix seconds), returning the number
    /// removed. Used to keep only a rolling retention window of history.
    pub async fn prune_listens_before(&self, cutoff_unix: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM listens WHERE listened_at_unix_seconds < ?1")
            .bind(cutoff_unix)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Deletes the entire listening history, returning the number of rows removed.
    /// Used by the user-facing "clear history" action (distinct from the rolling prune).
    pub async fn clear_listens(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM listens").execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    /// Queue a confirmed listen for outbound scrobbling (e.g. to ListenBrainz). The submit
    /// loop ([`pending_scrobbles`](Self::pending_scrobbles)) drains it independently.
    pub async fn enqueue_scrobble(&self, track_id: &str, listened_at: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO scrobble_queue (track_id, listened_at_unix_seconds) VALUES (?1, ?2)",
        )
        .bind(track_id)
        .bind(listened_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Oldest-first pending scrobbles `(queue_id, track_id, listened_at)`, up to `limit`.
    pub async fn pending_scrobbles(&self, limit: i64) -> Result<Vec<(i64, String, i64)>> {
        let rows = sqlx::query(
            "SELECT id, track_id, listened_at_unix_seconds FROM scrobble_queue
             ORDER BY id ASC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("id")?,
                    row.try_get("track_id")?,
                    row.try_get("listened_at_unix_seconds")?,
                ))
            })
            .collect()
    }

    /// Remove a scrobble from the queue once it's been accepted upstream.
    pub async fn delete_scrobble(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM scrobble_queue WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Tracks paired with their listen count, most played first. Ties break on recency.
    /// Counts confirmed listens only (skips are excluded).
    pub async fn most_played(&self, limit: usize) -> Result<Vec<(Track, i64)>> {
        self.most_played_since(limit, i64::MIN).await
    }

    /// Like [`most_played`](Self::most_played) but restricted to listens at or after
    /// `since_unix` — e.g. "top tracks in the last 30 days". Confirmed listens only.
    pub async fn most_played_since(
        &self,
        limit: usize,
        since_unix: i64,
    ) -> Result<Vec<(Track, i64)>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns}, COUNT(*) AS plays
             FROM listens l JOIN tracks t ON t.id = l.track_id
             WHERE l.event_kind = 'played' AND l.listened_at_unix_seconds >= ?2
             GROUP BY l.track_id
             ORDER BY plays DESC, MAX(l.listened_at_unix_seconds) DESC
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .bind(since_unix)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| Ok((track_from_row(row)?, row.try_get("plays")?)))
            .collect()
    }

    /// Tracks most often skipped, most-skipped first. Ties break on the most recent
    /// skip. Mirrors [`most_played`](Self::most_played) over the `skipped` rows.
    pub async fn most_skipped(&self, limit: usize) -> Result<Vec<(Track, i64)>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns}, COUNT(*) AS skips
             FROM listens l JOIN tracks t ON t.id = l.track_id
             WHERE l.event_kind = 'skipped'
             GROUP BY l.track_id
             ORDER BY skips DESC, MAX(l.listened_at_unix_seconds) DESC
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| Ok((track_from_row(row)?, row.try_get("skips")?)))
            .collect()
    }

    /// Library tracks with no confirmed listen in the retained history window, newest
    /// additions first. NOTE: history is pruned to a rolling window, so a track last
    /// played before the window fell off counts as "never played" here — this is
    /// "never played recently", not lifetime-never. Lifetime tracking would need an
    /// unpruned per-track counter.
    pub async fn never_played(&self, limit: usize) -> Result<Vec<Track>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns}
             FROM tracks t
             WHERE NOT EXISTS (
                 SELECT 1 FROM listens l
                 WHERE l.track_id = t.id AND l.event_kind = 'played'
             )
             ORDER BY t.added_at_unix_seconds DESC, t.id
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(track_from_row).collect()
    }

    /// Favorited tracks with no confirmed listen since `since_unix` — starred music
    /// you've drifted away from (rediscovery). Most-recently favorited first.
    pub async fn forgotten_favorites(
        &self,
        limit: usize,
        since_unix: i64,
    ) -> Result<Vec<Track>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns}
             FROM favorites f JOIN tracks t ON t.id = f.item_id
             WHERE f.item_type = 'track'
               AND NOT EXISTS (
                 SELECT 1 FROM listens l
                 WHERE l.track_id = t.id
                   AND l.event_kind = 'played'
                   AND l.listened_at_unix_seconds >= ?2
             )
             ORDER BY f.created_at_unix_seconds DESC, t.id
             LIMIT ?1"
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .bind(since_unix)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(track_from_row).collect()
    }

    /// Aggregate listening statistics as of `now_unix` (Unix seconds). `now_unix` is
    /// passed in rather than read from the clock so the result is deterministic for
    /// callers and tests. Totals come from cheap indexed counts; the daily streak and
    /// listening-session figures are derived in Rust from the ordered played-listen
    /// times (see [`streak_days`] and [`listening_sessions`]).
    pub async fn listening_stats(&self, now_unix: i64) -> Result<ListeningStats> {
        const DAY: i64 = 86_400;
        const SESSION_GAP_SECONDS: i64 = 30 * 60;

        let total_plays: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM listens WHERE event_kind = 'played'")
                .fetch_one(&self.pool)
                .await?;
        let total_skips: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM listens WHERE event_kind = 'skipped'")
                .fetch_one(&self.pool)
                .await?;
        let distinct_tracks_played: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT track_id) FROM listens WHERE event_kind = 'played'",
        )
        .fetch_one(&self.pool)
        .await?;
        let plays_last_7_days: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM listens
             WHERE event_kind = 'played' AND listened_at_unix_seconds >= ?1",
        )
        .bind(now_unix - 7 * DAY)
        .fetch_one(&self.pool)
        .await?;
        let plays_last_30_days: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM listens
             WHERE event_kind = 'played' AND listened_at_unix_seconds >= ?1",
        )
        .bind(now_unix - 30 * DAY)
        .fetch_one(&self.pool)
        .await?;

        // Ordered played times feed the streak + session derivations below.
        let times: Vec<i64> = sqlx::query_scalar(
            "SELECT listened_at_unix_seconds FROM listens
             WHERE event_kind = 'played' ORDER BY listened_at_unix_seconds ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        // Unique UTC day indices (times are sorted, so adjacent-dedup yields unique).
        let mut days: Vec<i64> = times.iter().map(|t| t.div_euclid(DAY)).collect();
        days.dedup();
        let (current_streak_days, longest_streak_days) =
            streak_days(&days, now_unix.div_euclid(DAY));
        let (sessions, longest_session_plays) = listening_sessions(&times, SESSION_GAP_SECONDS);

        let mut favorite_tracks = 0;
        let mut favorite_albums = 0;
        let mut favorite_artists = 0;
        let fav_rows =
            sqlx::query("SELECT item_type, COUNT(*) AS n FROM favorites GROUP BY item_type")
                .fetch_all(&self.pool)
                .await?;
        for row in &fav_rows {
            let item_type: String = row.try_get("item_type")?;
            let n: i64 = row.try_get("n")?;
            match item_type.as_str() {
                "track" => favorite_tracks = n,
                "album" => favorite_albums = n,
                "artist" => favorite_artists = n,
                _ => {}
            }
        }

        Ok(ListeningStats {
            total_plays,
            total_skips,
            distinct_tracks_played,
            plays_last_7_days,
            plays_last_30_days,
            current_streak_days,
            longest_streak_days,
            listening_sessions: sessions,
            longest_session_plays,
            favorite_tracks,
            favorite_albums,
            favorite_artists,
        })
    }

    // ---- Playlists ----

    /// Create a playlist (with optional initial tracks) and return its generated id.
    pub async fn create_playlist(
        &self,
        name: &str,
        comment: Option<&str>,
        track_ids: &[String],
        now: i64,
    ) -> Result<String> {
        let row = sqlx::query(
            "INSERT INTO playlists (id, name, comment, created_at_unix_seconds, updated_at_unix_seconds)
             VALUES (lower(hex(randomblob(12))), ?1, ?2, ?3, ?3) RETURNING id",
        )
        .bind(name)
        .bind(comment)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        let id: String = row.try_get("id")?;
        if !track_ids.is_empty() {
            self.set_playlist_tracks(&id, track_ids, now).await?;
        }
        Ok(id)
    }

    pub async fn list_playlists(&self) -> Result<Vec<Playlist>> {
        let rows = sqlx::query(
            "SELECT id, name, comment, created_at_unix_seconds, updated_at_unix_seconds,
                    (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = playlists.id) AS song_count
             FROM playlists ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(playlist_from_row).collect()
    }

    pub async fn playlist(&self, id: &str) -> Result<Option<Playlist>> {
        let row = sqlx::query(
            "SELECT id, name, comment, created_at_unix_seconds, updated_at_unix_seconds,
                    (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = playlists.id) AS song_count
             FROM playlists WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(playlist_from_row).transpose()
    }

    /// The playlist's tracks in order. Tracks no longer in the library are skipped.
    pub async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns} FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id
             WHERE pt.playlist_id = ?1 ORDER BY pt.position"
        );
        let rows = sqlx::query(&sql).bind(id).fetch_all(&self.pool).await?;
        rows.iter().map(track_from_row).collect()
    }

    /// The raw ordered track ids (including any that no longer resolve to a track).
    pub async fn playlist_track_ids(&self, id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| Ok(row.try_get("track_id")?))
            .collect()
    }

    pub async fn update_playlist_meta(
        &self,
        id: &str,
        name: Option<&str>,
        comment: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE playlists SET name = COALESCE(?2, name), \
             comment = COALESCE(?3, comment), updated_at_unix_seconds = ?4 WHERE id = ?1",
        )
        .bind(id)
        .bind(name)
        .bind(comment)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace the playlist's tracks with `track_ids`, in order.
    pub async fn set_playlist_tracks(
        &self,
        id: &str,
        track_ids: &[String],
        now: i64,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        for (position, track_id) in track_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
            )
            .bind(id)
            .bind(position as i64)
            .bind(track_id)
            .execute(&mut *conn)
            .await?;
        }
        sqlx::query("UPDATE playlists SET updated_at_unix_seconds = ?2 WHERE id = ?1")
            .bind(id)
            .bind(now)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM playlists WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Favorites (starred tracks / albums / artists) ----

    pub async fn set_favorite(&self, item_type: &str, item_id: &str, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO favorites (item_type, item_id, created_at_unix_seconds) VALUES (?1, ?2, ?3)
             ON CONFLICT(item_type, item_id) DO NOTHING",
        )
        .bind(item_type)
        .bind(item_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn clear_favorite(&self, item_type: &str, item_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM favorites WHERE item_type = ?1 AND item_id = ?2")
            .bind(item_type)
            .bind(item_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All favorited ids of a given type (for marking entities as starred).
    pub async fn favorite_ids(&self, item_type: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT item_id FROM favorites WHERE item_type = ?1")
            .bind(item_type)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|row| Ok(row.try_get("item_id")?)).collect()
    }

    pub async fn starred_tracks(&self) -> Result<Vec<Track>> {
        let columns = track_columns("t");
        let sql = format!(
            "SELECT {columns} FROM favorites f JOIN tracks t ON t.id = f.item_id
             WHERE f.item_type = 'track' ORDER BY f.created_at_unix_seconds DESC"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(track_from_row).collect()
    }

    pub async fn starred_albums(&self) -> Result<Vec<Album>> {
        let sql = format!(
            "SELECT {ALBUM_COLUMNS} FROM favorites f JOIN albums a ON a.id = f.item_id
             WHERE f.item_type = 'album' ORDER BY f.created_at_unix_seconds DESC"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(album_from_row).collect()
    }

    pub async fn starred_artists(&self) -> Result<Vec<Artist>> {
        let rows = sqlx::query(
            "SELECT a.id, a.name, a.album_count, a.track_count, a.artwork_url FROM favorites f
             JOIN artists a ON a.id = f.item_id
             WHERE f.item_type = 'artist' ORDER BY f.created_at_unix_seconds DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(artist_from_row).collect()
    }

    // ---- Internet radio stations ----

    pub async fn create_radio_station(
        &self,
        name: &str,
        stream_url: &str,
        homepage_url: Option<&str>,
        now: i64,
    ) -> Result<String> {
        let row = sqlx::query(
            "INSERT INTO radio_stations (id, name, stream_url, homepage_url, created_at_unix_seconds)
             VALUES (lower(hex(randomblob(12))), ?1, ?2, ?3, ?4) RETURNING id",
        )
        .bind(name)
        .bind(stream_url)
        .bind(homepage_url)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("id")?)
    }

    pub async fn list_radio_stations(&self) -> Result<Vec<RadioStation>> {
        let rows = sqlx::query(
            "SELECT id, name, stream_url, homepage_url, created_at_unix_seconds
             FROM radio_stations ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(radio_station_from_row).collect()
    }

    pub async fn radio_station(&self, id: &str) -> Result<Option<RadioStation>> {
        let row = sqlx::query(
            "SELECT id, name, stream_url, homepage_url, created_at_unix_seconds
             FROM radio_stations WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(radio_station_from_row).transpose()
    }

    pub async fn update_radio_station(
        &self,
        id: &str,
        name: Option<&str>,
        stream_url: Option<&str>,
        homepage_url: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE radio_stations SET name = COALESCE(?2, name), \
             stream_url = COALESCE(?3, stream_url), \
             homepage_url = COALESCE(?4, homepage_url) WHERE id = ?1",
        )
        .bind(id)
        .bind(name)
        .bind(stream_url)
        .bind(homepage_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_radio_station(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM radio_stations WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a configured source (e.g. an SMB share). The `id` is the provider id
    /// used to attribute that source's tracks; inserting an existing id replaces it.
    pub async fn add_source(&self, source: &SourceRecord) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO sources
                (id, kind, display_name, enabled, host, share, base_path, username, password, domain, created_at_unix_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(&source.id)
        .bind(&source.kind)
        .bind(&source.display_name)
        .bind(source.enabled as i64)
        .bind(&source.host)
        .bind(&source.share)
        .bind(&source.base_path)
        .bind(&source.username)
        .bind(&source.password)
        .bind(&source.domain)
        .bind(source.created_at_unix_seconds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let rows = sqlx::query(
            "SELECT id, kind, display_name, enabled, host, share, base_path, username, password, domain, created_at_unix_seconds
             FROM sources ORDER BY display_name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(source_from_row).collect()
    }

    pub async fn source(&self, id: &str) -> Result<Option<SourceRecord>> {
        let row = sqlx::query(
            "SELECT id, kind, display_name, enabled, host, share, base_path, username, password, domain, created_at_unix_seconds
             FROM sources WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(source_from_row).transpose()
    }

    pub async fn set_source_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        sqlx::query("UPDATE sources SET enabled = ?2 WHERE id = ?1")
            .bind(id)
            .bind(enabled as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_source(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sources WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load the persisted activity log (newest first), so history survives restart.
    pub async fn load_activities(&self) -> Result<Vec<ActivityRecord>> {
        let rows = sqlx::query(
            "SELECT id, kind, label, status, started_at_unix_seconds, finished_at_unix_seconds, message
             FROM activities ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(activity_from_row).collect()
    }

    /// Replace the persisted activity log with the current set (≤ a few dozen rows). Best-effort
    /// and idempotent (a full replace, re-run on every change), so on a transient "database is
    /// locked" — a long bulk write (the scan's `save_library`) holding the single WAL writer past
    /// the busy-timeout — retry a few times. Each attempt **releases the pooled connection**
    /// before backing off, so a blocked retry doesn't add to read-pool starvation (reads share
    /// the same pool). By the next attempt the bulk write has usually finished.
    pub async fn replace_activities(&self, activities: &[ActivityRecord]) -> Result<()> {
        let mut delay = std::time::Duration::from_millis(250);
        for attempt in 0..4u32 {
            match self.replace_activities_once(activities).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt < 3 && is_locked(&error) => {
                    tokio::time::sleep(delay).await;
                    delay *= 3;
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("the final attempt returns Ok or Err")
    }

    async fn replace_activities_once(&self, activities: &[ActivityRecord]) -> Result<(), sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result = async {
            sqlx::query("DELETE FROM activities")
                .execute(&mut *conn)
                .await?;
            for activity in activities {
                sqlx::query(
                    "INSERT INTO activities
                        (id, kind, label, status, started_at_unix_seconds, finished_at_unix_seconds, message)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .bind(activity.id)
                .bind(&activity.kind)
                .bind(&activity.label)
                .bind(&activity.status)
                .bind(activity.started_at_unix_seconds)
                .bind(activity.finished_at_unix_seconds)
                .bind(&activity.message)
                .execute(&mut *conn)
                .await?;
            }
            Ok::<(), sqlx::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }
}

/// Whether a sqlx error is a SQLite "database is locked"/"busy" — i.e. retryable contention
/// (SQLITE_BUSY = 5, SQLITE_LOCKED = 6) rather than a real failure.
fn is_locked(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "5" || code == "6")
}

/// A persisted background activity (a library scan/rescan and its outcome). Mirrors
/// the server's in-memory activity entry so the log survives restarts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityRecord {
    pub id: i64,
    pub kind: String,
    pub label: String,
    pub status: String,
    pub started_at_unix_seconds: i64,
    pub finished_at_unix_seconds: Option<i64>,
    pub message: Option<String>,
}

/// A user account. `password_hash` is an argon2 PHC string (hashing lives in the server
/// crate); `api_token` is a random per-user secret for Subsonic/API clients, cleartext at rest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub api_token: String,
    pub created_at_unix_seconds: i64,
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<UserRecord> {
    Ok(UserRecord {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        password_hash: row.try_get("password_hash")?,
        role: row.try_get("role")?,
        api_token: row.try_get("api_token")?,
        created_at_unix_seconds: row.try_get("created_at_unix_seconds")?,
    })
}

/// A persisted music source beyond the local-disk root. Today this is an SMB
/// share; the columns are a superset so other networked sources can reuse it.
/// `id` is the provider id used to attribute the source's tracks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRecord {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub enabled: bool,
    pub host: Option<String>,
    pub share: Option<String>,
    pub base_path: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub domain: Option<String>,
    pub created_at_unix_seconds: i64,
}

/// A persisted player registration (its live state lives in the player manager).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerRecord {
    pub id: String,
    pub kind: String,
    pub address: String,
    pub name: String,
    pub zone_id: Option<String>,
}

fn player_record_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<PlayerRecord> {
    Ok(PlayerRecord {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        address: row.try_get("address")?,
        name: row.try_get("name")?,
        zone_id: row.try_get("zone_id")?,
    })
}

/// How a playback event counts in listening history: a confirmed listen or a skip.
/// Stored as the `event_kind` text column on `listens`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenKind {
    /// The track played past the ListenBrainz threshold (a real listen).
    Played,
    /// The track was abandoned before the threshold (after a small noise floor).
    Skipped,
}

impl ListenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ListenKind::Played => "played",
            ListenKind::Skipped => "skipped",
        }
    }
}

/// Aggregate listening statistics for the history "stats" view. Produced by
/// [`Database::listening_stats`]. All counts are over the retained history window
/// (history is pruned), so totals are "recent", not lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListeningStats {
    /// Confirmed listens (`played` events) in history.
    pub total_plays: i64,
    /// Skip events in history.
    pub total_skips: i64,
    /// Distinct tracks with at least one confirmed listen.
    pub distinct_tracks_played: i64,
    pub plays_last_7_days: i64,
    pub plays_last_30_days: i64,
    /// Consecutive UTC days, ending today (or yesterday, until today logs a play),
    /// each with at least one confirmed listen.
    pub current_streak_days: i64,
    /// Longest run of consecutive UTC days with a confirmed listen, ever in history.
    pub longest_streak_days: i64,
    /// Number of listening sessions — runs of plays each within 30 minutes of the
    /// previous one.
    pub listening_sessions: i64,
    /// The most plays in any single session.
    pub longest_session_plays: i64,
    pub favorite_tracks: i64,
    pub favorite_albums: i64,
    pub favorite_artists: i64,
}

/// Given ascending, unique UTC day indices and the index of "today", returns
/// `(current_streak, longest_streak)` in days. The current streak counts back from
/// the most recent active day **only if** that day is today or yesterday (so a streak
/// isn't reported broken until a whole day passes with no play); otherwise it is 0.
fn streak_days(days: &[i64], today: i64) -> (i64, i64) {
    if days.is_empty() {
        return (0, 0);
    }
    let mut longest = 1i64;
    let mut run = 1i64;
    for window in days.windows(2) {
        if window[1] == window[0] + 1 {
            run += 1;
        } else {
            run = 1;
        }
        longest = longest.max(run);
    }

    let last = *days.last().expect("non-empty");
    let mut current = 0i64;
    if last == today || last == today - 1 {
        let mut expected = last;
        for &day in days.iter().rev() {
            if day == expected {
                current += 1;
                expected -= 1;
            } else if day < expected {
                break;
            }
        }
    }
    (current, longest)
}

/// Given ascending play times (Unix seconds) and a max inter-play `gap`, returns
/// `(session_count, max_plays_in_a_session)`. A session is a maximal run of plays each
/// within `gap` seconds of the previous one.
fn listening_sessions(times: &[i64], gap: i64) -> (i64, i64) {
    if times.is_empty() {
        return (0, 0);
    }
    let mut count = 1i64;
    let mut run = 1i64;
    let mut longest = 1i64;
    for window in times.windows(2) {
        if window[1] - window[0] <= gap {
            run += 1;
        } else {
            count += 1;
            run = 1;
        }
        longest = longest.max(run);
    }
    (count, longest)
}

/// Aggregate per-track loudness into one album figure. Each entry is `(integrated_lufs,
/// duration_seconds, true_peak_dbtp)`. The album loudness is the **duration-weighted mean**
/// of the tracks' loudness, computed in the linear domain and converted back to LUFS — the
/// standard approximation, since exact album-integrated R128 can't be recovered from
/// per-track values. The album true-peak is the max across its tracks. Empty input yields
/// `(None, None)`; a track with no/zero duration is weighted as 1.
fn album_loudness_from_tracks(
    tracks: &[(f64, Option<f64>, Option<f64>)],
) -> (Option<f64>, Option<f64>) {
    if tracks.is_empty() {
        return (None, None);
    }
    let mut energy_sum = 0.0f64;
    let mut weight_sum = 0.0f64;
    let mut peak: Option<f64> = None;
    for &(lufs, duration, track_peak) in tracks {
        let weight = duration.filter(|d| *d > 0.0).unwrap_or(1.0);
        energy_sum += weight * 10f64.powf(lufs / 10.0);
        weight_sum += weight;
        if let Some(p) = track_peak {
            peak = Some(peak.map_or(p, |existing: f64| existing.max(p)));
        }
    }
    let lufs = (weight_sum > 0.0).then(|| 10.0 * (energy_sum / weight_sum).log10());
    (lufs, peak)
}

/// The audio embedding dimension (PANNs CNN14) — matches the `track_embedding` vec0 column.
pub const AUDIO_EMBEDDING_DIM: usize = 2048;

/// Serialize an embedding to the little-endian f32 blob that `vec0` stores.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|value| value.to_le_bytes()).collect()
}

/// Turn similarity-ranked `(id, artist)` candidates (nearest first) into a varied station of up
/// to `limit` ids: **round-robin across artists** so no artist plays back-to-back and no single
/// artist dominates, while the most-similar artists still lead. The per-artist cap scales with
/// how many distinct artists the pool has — with few artists each contributes more (so the
/// station still fills); with many, ~one or two each (maximum variety). Within an artist the
/// nearest tracks come first. An empty/unknown artist is treated as its own (so unknowns don't
/// collapse together).
fn diversify(ranked: &[(String, String)], limit: usize) -> Vec<String> {
    use std::collections::{HashMap, VecDeque};
    let mut order: Vec<String> = Vec::new();
    let mut queues: HashMap<String, VecDeque<String>> = HashMap::new();
    for (id, artist) in ranked {
        let key = if artist.is_empty() { id.clone() } else { artist.clone() };
        if !queues.contains_key(&key) {
            order.push(key.clone());
        }
        queues.entry(key).or_default().push_back(id.clone());
    }

    let artists = order.len().max(1);
    let per_artist = limit.div_ceil(artists).max(1);

    let mut out = Vec::with_capacity(limit.min(ranked.len()));
    let mut used: HashMap<String, usize> = HashMap::new();
    loop {
        let before = out.len();
        for key in &order {
            if out.len() >= limit {
                return out;
            }
            let count = used.entry(key.clone()).or_insert(0);
            if *count >= per_artist {
                continue;
            }
            if let Some(id) = queues.get_mut(key).and_then(VecDeque::pop_front) {
                out.push(id);
                *count += 1;
            }
        }
        if out.len() == before {
            break; // every queue exhausted or capped
        }
    }
    out
}

/// The lightweight, persisted playback row for a player's server-owned queue —
/// everything except the queue items themselves.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerPlayback {
    pub status: PlaybackStatus,
    pub position: Option<usize>,
    pub elapsed_seconds: Option<f64>,
    pub volume: Option<u8>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
}

/// A player's full persisted queue: the playback row plus the ordered items.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerQueueSnapshot {
    pub playback: PlayerPlayback,
    pub items: Vec<QueueItem>,
}

/// An album that needs a cover fetched from an external artwork provider.
#[derive(Clone, Debug, PartialEq)]
pub struct AlbumArtworkTarget {
    pub album_id: String,
    pub artist_name: String,
    pub title: String,
}

/// An artist that needs an image fetched from an external provider (name-based).
#[derive(Clone, Debug, PartialEq)]
pub struct ArtistArtworkTarget {
    pub artist_id: String,
    pub name: String,
}

/// One user-curated artist-merge row: a member name key folded into a canonical artist.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtistAlias {
    pub alias_key: String,
    pub canonical_key: String,
    pub canonical_name: String,
}

/// A track that needs audio fingerprinting (to resolve MusicBrainz ids). Carries enough
/// to read and decode the file: the provider (SMB reads `provider_item_id`) and the
/// absolute `path` (local disk reads that).
#[derive(Clone, Debug, PartialEq)]
pub struct TrackFingerprintTarget {
    pub track_id: String,
    pub title: String,
    pub artist_name: String,
    pub extension: String,
    pub provider_id: String,
    pub provider_item_id: String,
    pub path: String,
    /// The track's full length in seconds (read at scan time). AcoustID filters matches by
    /// duration, so the lookup must report the *real* length, not the fingerprint window.
    pub duration_seconds: Option<f64>,
}

/// A track to identify by MusicBrainz text search (the AcoustID-fallback path), carrying
/// the tags the search query is built from.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSearchTarget {
    pub track_id: String,
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub year: Option<u16>,
}

/// Library-wide MusicBrainz identification coverage (for the `/admin` stats panel).
#[derive(Clone, Debug, PartialEq)]
pub struct IdentificationStats {
    pub tracks_total: i64,
    pub tracks_identified: i64,
    pub albums_total: i64,
    pub albums_identified: i64,
    pub artists_total: i64,
    pub artists_identified: i64,
    /// Tracks the background pass has attempted so far (resolved + not_found + searched);
    /// `tracks_total - tracks_processed` are still queued — the remaining work.
    pub tracks_processed: i64,
    pub fingerprint_resolved: i64,
    pub fingerprint_not_found: i64,
    pub fingerprint_searched: i64,
    /// Identified tracks whose full MusicBrainz metadata has been fetched + applied.
    pub tracks_enriched: i64,
    /// Albums / artists that have an acquired cover image.
    pub album_covers: i64,
    pub artist_images: i64,
}

/// SQL boolean: whether the track referenced by `track_id_expr` (e.g. `"t.id"`) carries a
/// MusicBrainz recording id — via an embedded-tag observation or a resolved fingerprint/
/// search. The single definition of "identified" used across the stats queries.
fn track_identified_clause(track_id_expr: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM track_metadata_observations o
                  WHERE o.track_id = {tid} AND o.musicbrainz_recording_id IS NOT NULL
                    AND o.musicbrainz_recording_id <> '')
          OR EXISTS (SELECT 1 FROM track_fingerprint f
                     WHERE f.track_id = {tid} AND f.status = 'resolved'))",
        tid = track_id_expr
    )
}

/// A track whose fingerprinted recording MBID is ready to enrich from MusicBrainz.
#[derive(Clone, Debug, PartialEq)]
pub struct MusicBrainzEnrichTarget {
    pub track_id: String,
    pub recording_mbid: String,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
}

/// Resolved MusicBrainz metadata for a track, stored in `track_musicbrainz_metadata` and
/// applied to the canonical library by `reapply_canonical_grouping`. Numbers are `i64`
/// for SQLite binding; `reapply` narrows them to `u16` for the track columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedMusicBrainzMetadata {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub album_artist_name: Option<String>,
    pub track_number: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_number: Option<i64>,
    pub recording_date: Option<String>,
    pub year: Option<i64>,
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
    pub artist_mbid: Option<String>,
}

/// The acquired-artwork record for an album (provenance + cache reference).
#[derive(Clone, Debug, PartialEq)]
pub struct AcquiredArtwork {
    pub provider: String,
    pub remote_url: Option<String>,
    pub cache_key: Option<String>,
    pub ext: Option<String>,
    pub width: Option<u32>,
    /// `acquired` (cover cached) or `not_found` (negative cache marker).
    pub status: String,
}

fn playback_status_str(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Stopped => "stopped",
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
    }
}

fn playback_status_from_str(value: &str) -> PlaybackStatus {
    match value {
        "playing" => PlaybackStatus::Playing,
        "paused" => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn repeat_mode_str(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

fn repeat_mode_from_str(value: &str) -> RepeatMode {
    match value {
        "all" => RepeatMode::All,
        "one" => RepeatMode::One,
        _ => RepeatMode::Off,
    }
}

const ALBUM_COLUMNS: &str =
    "id, title, artist_id, artist_name, year, track_count, artwork_url, artwork_path";

/// The track column list, prefixed with a table alias, matching `track_from_row`.
fn track_columns(alias: &str) -> String {
    [
        "id",
        "provider_id",
        "provider_item_id",
        "title",
        "artist_id",
        "artist_name",
        "album_id",
        "album_title",
        "year",
        "track_number",
        "disc_number",
        "extension",
        "file_size_bytes",
        "modified_at_unix_seconds",
        "content_hash",
        "relative_path",
        "stream_url",
        "added_at_unix_seconds",
        "duration_seconds",
        "path",
    ]
    .iter()
    .map(|column| format!("{alias}.{column}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn artist_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Artist> {
    Ok(Artist {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        album_count: i64_to_usize(row.try_get("album_count")?, "album_count")?,
        track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
        artwork_url: row.try_get("artwork_url")?,
    })
}

fn radio_station_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RadioStation> {
    Ok(RadioStation {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        stream_url: row.try_get("stream_url")?,
        homepage_url: row.try_get("homepage_url")?,
        created_at_unix_seconds: row.try_get("created_at_unix_seconds")?,
    })
}

fn activity_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ActivityRecord> {
    Ok(ActivityRecord {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        label: row.try_get("label")?,
        status: row.try_get("status")?,
        started_at_unix_seconds: row.try_get("started_at_unix_seconds")?,
        finished_at_unix_seconds: row.try_get("finished_at_unix_seconds")?,
        message: row.try_get("message")?,
    })
}

fn source_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SourceRecord> {
    Ok(SourceRecord {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        display_name: row.try_get("display_name")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        host: row.try_get("host")?,
        share: row.try_get("share")?,
        base_path: row.try_get("base_path")?,
        username: row.try_get("username")?,
        password: row.try_get("password")?,
        domain: row.try_get("domain")?,
        created_at_unix_seconds: row.try_get("created_at_unix_seconds")?,
    })
}

fn playlist_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Playlist> {
    Ok(Playlist {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        comment: row.try_get("comment")?,
        song_count: i64_to_usize(row.try_get("song_count")?, "song_count")?,
        created_at_unix_seconds: row.try_get("created_at_unix_seconds")?,
        updated_at_unix_seconds: row.try_get("updated_at_unix_seconds")?,
    })
}

fn album_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Album> {
    Ok(Album {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        artist_id: row.try_get("artist_id")?,
        artist_name: row.try_get("artist_name")?,
        year: optional_i64_to_u16(row.try_get("year")?, "year")?,
        track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
        artwork_url: row.try_get("artwork_url")?,
        artwork_path: row
            .try_get::<Option<String>, _>("artwork_path")?
            .map(PathBuf::from),
    })
}

/// Builds a `Track` from a row without observation provenance (`observed_metadata`
/// is empty). Used by the list/detail/stream read paths.
fn track_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Track> {
    Ok(Track {
        id: row.try_get("id")?,
        provider: ProviderMapping {
            provider_id: row.try_get("provider_id")?,
            item_id: row.try_get("provider_item_id")?,
        },
        observed_metadata: Vec::new(),
        title: row.try_get("title")?,
        artist_id: row.try_get("artist_id")?,
        artist_name: row.try_get("artist_name")?,
        album_id: row.try_get("album_id")?,
        album_title: row.try_get("album_title")?,
        year: optional_i64_to_u16(row.try_get("year")?, "year")?,
        track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
        disc_number: optional_i64_to_u16(row.try_get("disc_number")?, "disc_number")?,
        extension: row.try_get("extension")?,
        file_size_bytes: optional_i64_to_u64(row.try_get("file_size_bytes")?, "file_size_bytes")?,
        duration_seconds: row.try_get("duration_seconds")?,
        modified_at_unix_seconds: row.try_get("modified_at_unix_seconds")?,
        content_hash: row.try_get("content_hash")?,
        relative_path: row.try_get("relative_path")?,
        stream_url: row.try_get("stream_url")?,
        added_at_unix_seconds: row.try_get("added_at_unix_seconds")?,
        path: PathBuf::from(row.try_get::<String, _>("path")?),
    })
}

// Allowlisted ORDER BY fragments (no user input is interpolated). Kept in sync
// with the previous in-memory sort_* helpers in the server.
fn artist_order_clause(sort: Option<&str>) -> &'static str {
    match sort {
        Some("tracks") => "track_count DESC, name",
        Some("albums") => "album_count DESC, name",
        _ => "name",
    }
}

fn album_order_clause(sort: Option<&str>) -> &'static str {
    match sort {
        Some("title") => "title",
        Some("year") => "year DESC, artist_name, title",
        _ => "artist_name, year, title",
    }
}

fn track_order_clause(sort: Option<&str>) -> &'static str {
    match sort {
        Some("title") => "t.title",
        Some("album") => "t.album_title, t.disc_number, t.track_number",
        Some("recent") => "t.added_at_unix_seconds DESC, t.artist_name, t.title",
        _ => "t.artist_name, t.year, t.album_title, t.disc_number, t.track_number, t.title",
    }
}

/// Builds a safe FTS5 MATCH expression from free-form user input: each
/// alphanumeric token becomes a quoted prefix term joined by implicit AND, e.g.
/// `daft punk` -> `"daft"* "punk"*`. Quoting neutralizes FTS operator characters,
/// and prefix matching supports type-ahead. Returns `None` when the input has no
/// usable tokens.
fn fts_match_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryChangeSet {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
}

impl LibraryChangeSet {
    pub fn has_changes(self) -> bool {
        self.added > 0 || self.removed > 0 || self.modified > 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackFingerprint {
    file_size_bytes: Option<u64>,
    modified_at_unix_seconds: Option<i64>,
    content_hash: Option<String>,
    metadata_observations: String,
}

fn metadata_observations_fingerprint(observations: &[TrackMetadataObservation]) -> String {
    let mut fingerprint = String::new();

    for observation in observations {
        push_fingerprint_part(&mut fingerprint, &observation.source);
        push_fingerprint_part(&mut fingerprint, &observation.confidence.to_string());
        // `observed_at_unix_seconds` is intentionally excluded: it is stamped fresh
        // on every scan, so including it would mark every track "modified" whenever
        // two scans straddle a second boundary. Change detection tracks metadata
        // content, not when it was observed.
        push_fingerprint_part(
            &mut fingerprint,
            approval_state_to_str(&observation.approval_state),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.title.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.artist_name.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.album_artist_name.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.album_title.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.recording_date.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation
                .year
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation
                .track_number
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation
                .track_total
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation
                .disc_number
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation
                .disc_total
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        push_fingerprint_part(&mut fingerprint, &observation.genres.join("\n"));
        push_fingerprint_part(&mut fingerprint, &observation.composers.join("\n"));
        push_fingerprint_part(
            &mut fingerprint,
            observation.lyrics.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_recording_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_track_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_release_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_release_group_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_artist_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation
                .musicbrainz_release_artist_id
                .as_deref()
                .unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            observation.isrc.as_deref().unwrap_or_default(),
        );
        push_fingerprint_part(
            &mut fingerprint,
            &observation.embedded_artwork_count.to_string(),
        );

        let field_observations = observation.effective_field_observations();
        push_fingerprint_part(&mut fingerprint, &field_observations.len().to_string());
        for field_observation in field_observations {
            push_fingerprint_part(&mut fingerprint, &field_observation.source);
            push_fingerprint_part(&mut fingerprint, &field_observation.field_name);
            push_fingerprint_part(
                &mut fingerprint,
                &metadata_field_value_fingerprint(&field_observation.value),
            );
            push_fingerprint_part(&mut fingerprint, &field_observation.confidence.to_string());
            // Excluded for the same reason as the observation-level timestamp above.
            push_fingerprint_part(
                &mut fingerprint,
                approval_state_to_str(&field_observation.approval_state),
            );
        }
    }

    fingerprint
}

fn metadata_field_value_fingerprint(value: &MetadataFieldValue) -> String {
    let mut fingerprint = String::new();

    match value {
        MetadataFieldValue::Text(value) => {
            push_fingerprint_part(&mut fingerprint, "text");
            push_fingerprint_part(&mut fingerprint, value);
        }
        MetadataFieldValue::Number(value) => {
            push_fingerprint_part(&mut fingerprint, "number");
            push_fingerprint_part(&mut fingerprint, &value.to_string());
        }
        MetadataFieldValue::TextList(values) => {
            push_fingerprint_part(&mut fingerprint, "text_list");
            push_fingerprint_part(&mut fingerprint, &values.len().to_string());
            for value in values {
                push_fingerprint_part(&mut fingerprint, value);
            }
        }
        MetadataFieldValue::Count(value) => {
            push_fingerprint_part(&mut fingerprint, "count");
            push_fingerprint_part(&mut fingerprint, &value.to_string());
        }
    }

    fingerprint
}

fn push_fingerprint_part(fingerprint: &mut String, value: &str) {
    fingerprint.push_str(&value.len().to_string());
    fingerprint.push(':');
    fingerprint.push_str(value);
    fingerprint.push(';');
}

const MIGRATION_001: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS providers (
        id TEXT PRIMARY KEY,
        source_root TEXT NOT NULL,
        scanned_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS artists (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        album_count INTEGER NOT NULL,
        track_count INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS albums (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        artist_id TEXT NOT NULL,
        artist_name TEXT NOT NULL,
        year INTEGER,
        track_count INTEGER NOT NULL,
        artwork_url TEXT,
        artwork_path TEXT
    )",
    "CREATE TABLE IF NOT EXISTS tracks (
        id TEXT PRIMARY KEY,
        provider_id TEXT NOT NULL,
        provider_item_id TEXT NOT NULL,
        title TEXT NOT NULL,
        artist_id TEXT NOT NULL,
        artist_name TEXT NOT NULL,
        album_id TEXT NOT NULL,
        album_title TEXT NOT NULL,
        year INTEGER,
        track_number INTEGER,
        disc_number INTEGER,
        extension TEXT NOT NULL,
        file_size_bytes INTEGER,
        modified_at_unix_seconds INTEGER,
        content_hash TEXT,
        relative_path TEXT NOT NULL,
        stream_url TEXT NOT NULL,
        path TEXT NOT NULL,
        added_at_unix_seconds INTEGER
    )",
    "CREATE TABLE IF NOT EXISTS scan_errors (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        path TEXT NOT NULL,
        message TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS track_metadata_observations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        track_id TEXT NOT NULL,
        source TEXT NOT NULL,
        confidence REAL NOT NULL DEFAULT 0,
        observed_at_unix_seconds INTEGER NOT NULL DEFAULT 0,
        approval_state TEXT NOT NULL DEFAULT 'observed',
        title TEXT,
        artist_name TEXT,
        album_artist_name TEXT,
        album_title TEXT,
        recording_date TEXT,
        year INTEGER,
        track_number INTEGER,
        track_total INTEGER,
        disc_number INTEGER,
        disc_total INTEGER,
        genres TEXT,
        composers TEXT,
        lyrics TEXT,
        musicbrainz_recording_id TEXT,
        musicbrainz_track_id TEXT,
        musicbrainz_release_id TEXT,
        musicbrainz_release_group_id TEXT,
        musicbrainz_artist_id TEXT,
        musicbrainz_release_artist_id TEXT,
        isrc TEXT,
        embedded_artwork_count INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE TABLE IF NOT EXISTS track_metadata_field_observations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        observation_id INTEGER NOT NULL,
        track_id TEXT NOT NULL,
        source TEXT NOT NULL,
        field_name TEXT NOT NULL,
        value_json TEXT NOT NULL,
        confidence REAL NOT NULL DEFAULT 0,
        observed_at_unix_seconds INTEGER NOT NULL DEFAULT 0,
        approval_state TEXT NOT NULL DEFAULT 'observed',
        FOREIGN KEY(observation_id) REFERENCES track_metadata_observations(id) ON DELETE CASCADE,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id)",
    "CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id)",
    "CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id)",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_observations_track_id ON track_metadata_observations(track_id)",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_field_observations_observation_id ON track_metadata_field_observations(observation_id)",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_field_observations_track_id ON track_metadata_field_observations(track_id)",
];

struct ColumnMigration {
    column: &'static str,
    alter_statement: &'static str,
}

const MIGRATION_002_TRACK_COLUMNS: &[ColumnMigration] = &[
    ColumnMigration {
        column: "file_size_bytes",
        alter_statement: "ALTER TABLE tracks ADD COLUMN file_size_bytes INTEGER",
    },
    ColumnMigration {
        column: "modified_at_unix_seconds",
        alter_statement: "ALTER TABLE tracks ADD COLUMN modified_at_unix_seconds INTEGER",
    },
    ColumnMigration {
        column: "content_hash",
        alter_statement: "ALTER TABLE tracks ADD COLUMN content_hash TEXT",
    },
];

const MIGRATION_003: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS track_metadata_observations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        track_id TEXT NOT NULL,
        source TEXT NOT NULL,
        confidence REAL NOT NULL DEFAULT 0,
        observed_at_unix_seconds INTEGER NOT NULL DEFAULT 0,
        approval_state TEXT NOT NULL DEFAULT 'observed',
        title TEXT,
        artist_name TEXT,
        album_artist_name TEXT,
        album_title TEXT,
        recording_date TEXT,
        year INTEGER,
        track_number INTEGER,
        track_total INTEGER,
        disc_number INTEGER,
        disc_total INTEGER,
        genres TEXT,
        composers TEXT,
        lyrics TEXT,
        musicbrainz_recording_id TEXT,
        musicbrainz_track_id TEXT,
        musicbrainz_release_id TEXT,
        musicbrainz_release_group_id TEXT,
        musicbrainz_artist_id TEXT,
        musicbrainz_release_artist_id TEXT,
        isrc TEXT,
        embedded_artwork_count INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_observations_track_id ON track_metadata_observations(track_id)",
];

const MIGRATION_004_METADATA_OBSERVATION_COLUMNS: &[ColumnMigration] = &[
    ColumnMigration {
        column: "confidence",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN confidence REAL NOT NULL DEFAULT 0",
    },
    ColumnMigration {
        column: "observed_at_unix_seconds",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN observed_at_unix_seconds INTEGER NOT NULL DEFAULT 0",
    },
    ColumnMigration {
        column: "approval_state",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN approval_state TEXT NOT NULL DEFAULT 'observed'",
    },
];

const MIGRATION_005_METADATA_OBSERVATION_VALUE_COLUMNS: &[ColumnMigration] = &[
    ColumnMigration {
        column: "album_artist_name",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN album_artist_name TEXT",
    },
    ColumnMigration {
        column: "recording_date",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN recording_date TEXT",
    },
    ColumnMigration {
        column: "track_total",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN track_total INTEGER",
    },
    ColumnMigration {
        column: "disc_number",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN disc_number INTEGER",
    },
    ColumnMigration {
        column: "disc_total",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN disc_total INTEGER",
    },
    ColumnMigration {
        column: "genres",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN genres TEXT",
    },
    ColumnMigration {
        column: "composers",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN composers TEXT",
    },
    ColumnMigration {
        column: "lyrics",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN lyrics TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_recording_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_recording_id TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_track_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_track_id TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_release_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_release_id TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_release_group_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_release_group_id TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_artist_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_artist_id TEXT",
    },
    ColumnMigration {
        column: "musicbrainz_release_artist_id",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN musicbrainz_release_artist_id TEXT",
    },
    ColumnMigration {
        column: "isrc",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN isrc TEXT",
    },
    ColumnMigration {
        column: "embedded_artwork_count",
        alter_statement: "ALTER TABLE track_metadata_observations ADD COLUMN embedded_artwork_count INTEGER NOT NULL DEFAULT 0",
    },
];

const MIGRATION_007_TRACK_DISC_NUMBER: &[ColumnMigration] = &[ColumnMigration {
    column: "disc_number",
    alter_statement: "ALTER TABLE tracks ADD COLUMN disc_number INTEGER",
}];

const MIGRATION_008_TRACK_ADDED_AT: &[ColumnMigration] = &[ColumnMigration {
    column: "added_at_unix_seconds",
    alter_statement: "ALTER TABLE tracks ADD COLUMN added_at_unix_seconds INTEGER",
}];

const MIGRATION_012_TRACK_DURATION: &[ColumnMigration] = &[ColumnMigration {
    column: "duration_seconds",
    alter_statement: "ALTER TABLE tracks ADD COLUMN duration_seconds REAL",
}];

// User playlists (ordered track lists) and favorites (starred tracks/albums/artists).
// Playlist/favorite rows reference track/album/artist ids; entries for items no longer
// in the library are skipped by the joins, mirroring listening history.
const MIGRATION_013_PLAYLISTS_AND_FAVORITES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS playlists (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        comment TEXT,
        created_at_unix_seconds INTEGER NOT NULL,
        updated_at_unix_seconds INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS playlist_tracks (
        playlist_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        track_id TEXT NOT NULL,
        PRIMARY KEY (playlist_id, position),
        FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist ON playlist_tracks(playlist_id)",
    "CREATE TABLE IF NOT EXISTS favorites (
        item_type TEXT NOT NULL,
        item_id TEXT NOT NULL,
        created_at_unix_seconds INTEGER NOT NULL,
        PRIMARY KEY (item_type, item_id)
    )",
];

// Internet radio stations — the first non-library music source (see docs/plugins.md).
const MIGRATION_014_RADIO_STATIONS: &[&str] = &["CREATE TABLE IF NOT EXISTS radio_stations (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        stream_url TEXT NOT NULL,
        homepage_url TEXT,
        created_at_unix_seconds INTEGER NOT NULL
    )"];

// Configured music sources beyond the local-disk root (which still comes from
// `--library`): network shares such as SMB. `id` is the provider id used to
// attribute tracks. Credentials are stored in the clear for now — HARDEN IN M12,
// mirroring the OpenSubsonic password handling.
// Background-activity log (library scans, rescans) persisted so the admin page's
// history — and any interrupted jobs — survive a restart.
const MIGRATION_016_ACTIVITIES: &[&str] = &["CREATE TABLE IF NOT EXISTS activities (
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        label TEXT NOT NULL,
        status TEXT NOT NULL,
        started_at_unix_seconds INTEGER NOT NULL,
        finished_at_unix_seconds INTEGER,
        message TEXT
    )"];

const MIGRATION_015_SOURCES: &[&str] = &["CREATE TABLE IF NOT EXISTS sources (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        display_name TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        host TEXT,
        share TEXT,
        base_path TEXT,
        username TEXT,
        password TEXT,
        domain TEXT,
        created_at_unix_seconds INTEGER NOT NULL
    )"];

// Registered players and zones. Players are reported to the server (e.g. via the
// web UI) and persisted so they survive restarts; a player optionally belongs to
// a zone (a named group used as a control target).
const MIGRATION_010_PLAYERS_AND_ZONES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS zones (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS players (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        address TEXT NOT NULL,
        name TEXT NOT NULL,
        zone_id TEXT,
        created_at INTEGER NOT NULL,
        FOREIGN KEY(zone_id) REFERENCES zones(id) ON DELETE SET NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_players_zone_id ON players(zone_id)",
];

// Server-owned persistent playback queue, one queue per player. `player_queue`
// holds the lightweight playback row (status/position/elapsed/volume/repeat/
// shuffle) updated frequently; `player_queue_items` holds the ordered queue and
// is rewritten only when the queue itself changes. On restart the manager reloads
// these so a queue survives a server restart (not just a page refresh).
const MIGRATION_017_PLAYER_QUEUE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS player_queue (
        player_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        position INTEGER,
        elapsed_seconds REAL,
        volume INTEGER,
        repeat TEXT NOT NULL,
        shuffle INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS player_queue_items (
        player_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        track_id TEXT,
        title TEXT NOT NULL,
        artist TEXT NOT NULL,
        album TEXT NOT NULL,
        stream_url TEXT NOT NULL,
        artwork_url TEXT,
        PRIMARY KEY (player_id, seq)
    )",
];

// Server-owned persistent playback queue, one per *zone*. A zone owns a canonical
// queue (members are outputs it drives), so it persists exactly like a player —
// same row shapes as `player_queue`/`player_queue_items`, keyed by `zone_id`. Kept
// in separate tables rather than overloading the player tables so player and zone
// id keyspaces never collide and `delete_player`/`delete_zone` cleanup stays clear.
const MIGRATION_018_ZONE_QUEUE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS zone_queue (
        zone_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        position INTEGER,
        elapsed_seconds REAL,
        volume INTEGER,
        repeat TEXT NOT NULL,
        shuffle INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS zone_queue_items (
        zone_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        track_id TEXT,
        title TEXT NOT NULL,
        artist TEXT NOT NULL,
        album TEXT NOT NULL,
        stream_url TEXT NOT NULL,
        artwork_url TEXT,
        PRIMARY KEY (zone_id, seq)
    )",
];

// Album cover art acquired from an external artwork provider (iTunes/Deezer/Cover Art
// Archive/fanart.tv). The image bytes live in the artwork cache (`cache_key`); this row
// records the provenance and lets the serve handler find them, plus a `not_found` marker
// so the periodic rescan doesn't re-query a coverless album every 30s. Deliberately
// **no foreign key to albums**: `save_library` rewrites the albums table every scan, and
// a cascade would wipe acquired covers — `album_id` is stable across rescans, so a
// re-added album keeps its art. `status` is `acquired` or `not_found`.
const MIGRATION_019_ACQUIRED_ARTWORK: &[&str] =
    &["CREATE TABLE IF NOT EXISTS acquired_album_artwork (
        album_id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        remote_url TEXT,
        cache_key TEXT,
        ext TEXT,
        width INTEGER,
        status TEXT NOT NULL,
        acquired_at_unix_seconds INTEGER NOT NULL
    )"];

// App settings the user edits in the web UI (not flags/config files): a small key/value
// store, e.g. whether to auto-fetch artwork and an optional fanart.tv key.
const MIGRATION_020_SETTINGS: &[&str] = &["CREATE TABLE IF NOT EXISTS settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )"];

// AcoustID audio-fingerprint results: MusicBrainz ids resolved by fingerprinting a
// track's audio (for files whose tags carry no MBIDs). Like acquired_album_artwork,
// deliberately **no foreign key** — `save_library` rewrites tracks/observations every
// scan, and `track_id` is stable across rescans. `status` is `resolved` or `not_found`
// (a negative-cache marker so the rescan doesn't re-fingerprint an unidentified track).
const MIGRATION_021_TRACK_FINGERPRINT: &[&str] = &["CREATE TABLE IF NOT EXISTS track_fingerprint (
        track_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        musicbrainz_recording_id TEXT,
        musicbrainz_release_id TEXT,
        musicbrainz_release_group_id TEXT,
        fingerprinted_at_unix_seconds INTEGER NOT NULL
    )"];

// Per-track EBU R128 loudness, in its own table (like track_fingerprint) so it survives a
// rescan that rewrites the tracks table. `integrated_lufs` is the whole-track loudness;
// `true_peak_dbtp` is the max oversampled peak — both feed the playback leveling gain.
const MIGRATION_026_TRACK_LOUDNESS: &[&str] = &["CREATE TABLE IF NOT EXISTS track_loudness (
        track_id TEXT PRIMARY KEY,
        integrated_lufs REAL,
        true_peak_dbtp REAL,
        analyzed_at_unix_seconds INTEGER NOT NULL
    )"];

// Cached external similarity results (ListenBrainz Labs), keyed by seed MBID. `payload_json`
// is a scored MBID list (an empty array is a `not_found` marker). Lets recommendations resolve
// off the cache instead of the network on the request/refill path.
const MIGRATION_027_SIMILARITY_CACHE: &[&str] = &["CREATE TABLE IF NOT EXISTS similarity_cache (
        entity_kind TEXT NOT NULL,
        seed_mbid TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        fetched_at_unix_seconds INTEGER NOT NULL,
        PRIMARY KEY (entity_kind, seed_mbid)
    )"];

// User accounts + login sessions. `password_hash` is an argon2 PHC string; `api_token` is a
// random per-user secret for Subsonic/programmatic clients, stored in cleartext because
// Subsonic's salted-token auth needs to recompute md5(token+salt) server-side (the same
// plaintext-secret-at-rest reality as SMB/Subsonic passwords — see roadmap M12). Sessions store
// the **sha256** of the cookie token, so a DB leak yields no live sessions.
const MIGRATION_028_USERS_AND_SESSIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        username TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        role TEXT NOT NULL DEFAULT 'listener',
        api_token TEXT NOT NULL,
        created_at_unix_seconds INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS sessions (
        token_hash TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        created_at_unix_seconds INTEGER NOT NULL,
        expires_at_unix_seconds INTEGER NOT NULL,
        user_agent TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)",
];

// Per-album aggregate loudness, derived from `track_loudness` (energy-weighted by track
// duration). Lets the browser apply album-mode volume leveling so an album's internal
// dynamics survive normalization. Recomputed after each loudness pass; FK-less so an album
// rewrite during a rescan can't cascade-delete it (it's re-derived anyway).
const MIGRATION_029_ALBUM_LOUDNESS: &[&str] = &["CREATE TABLE IF NOT EXISTS album_loudness (
        album_id TEXT PRIMARY KEY,
        integrated_lufs REAL,
        true_peak_dbtp REAL,
        analyzed_at_unix_seconds INTEGER NOT NULL
    )"];

// Per-player endpoint auth token (M10): the sha256 of a token issued at registration to a
// self-registering endpoint, so it can authenticate on its own channels without a user
// session. NULL for server-initiated players (browser/MPD/Snapcast), which carry no token.
const MIGRATION_030_PLAYER_AUTH_TOKEN: &[&str] =
    &["ALTER TABLE players ADD COLUMN auth_token_hash TEXT"];

/// Audio-ML (musicata-ml): a `vec0` virtual table holds each track's embedding for KNN
/// "sounds-like" similarity (sqlite-vec is loaded as standard — see `register_sqlite_vec`); a
/// companion table holds the model provenance + AudioSet tags JSON. Both FK-less and
/// re-derivable. The embedding dim ([`AUDIO_EMBEDDING_DIM`]) is the PANNs CNN14 model's 2048.
// Outbound scrobble queue: confirmed `played` listens awaiting submission to an external
// service (ListenBrainz). A decoupled queue so the submit loop drains at its own pace and a
// network outage never stalls playback/recording. No FK (`track_id` is stable; the row carries
// enough to resolve the track at submit time). Rows are deleted once accepted upstream.
const MIGRATION_032_SCROBBLE_QUEUE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS scrobble_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        track_id TEXT NOT NULL,
        listened_at_unix_seconds INTEGER NOT NULL
    )",
];

const MIGRATION_031_AUDIO_EMBEDDING: &[&str] = &[
    "CREATE VIRTUAL TABLE IF NOT EXISTS track_embedding USING vec0(
        track_id TEXT PRIMARY KEY,
        embedding float[2048] distance_metric=cosine
    )",
    "CREATE TABLE IF NOT EXISTS track_features (
        track_id TEXT PRIMARY KEY,
        model TEXT NOT NULL,
        version TEXT,
        tags_json TEXT,
        analyzed_at_unix_seconds INTEGER NOT NULL
    )",
];

// MusicBrainz metadata fetched for a track via its (fingerprinted) recording MBID —
// real title/artist/album/track-number, applied to the canonical library so an untagged
// track stops showing folder-derived junk. Like track_fingerprint / acquired_album_artwork,
// deliberately **no foreign key** (`track_id` is stable across the per-scan track rewrite),
// and `status` is `resolved` or `not_found` (negative cache so an unresolvable recording
// isn't re-queried every rescan). Applied to `tracks` by `reapply_canonical_grouping`.
const MIGRATION_022_TRACK_MUSICBRAINZ_METADATA: &[&str] =
    &["CREATE TABLE IF NOT EXISTS track_musicbrainz_metadata (
        track_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        title TEXT,
        artist_name TEXT,
        album_title TEXT,
        album_artist_name TEXT,
        track_number INTEGER,
        track_total INTEGER,
        disc_number INTEGER,
        recording_date TEXT,
        year INTEGER,
        musicbrainz_recording_id TEXT,
        musicbrainz_release_id TEXT,
        musicbrainz_release_group_id TEXT,
        musicbrainz_artist_id TEXT,
        enriched_at_unix_seconds INTEGER NOT NULL
    )"];

// Listening history. One row per playback event (see `players::ListenTracker`). A
// `played` row is a confirmed listen — a track played past the ListenBrainz threshold
// (`min(duration/2, 240s)`); a `skipped` row is a track abandoned before that (the
// `event_kind` column is added by MIGRATION_023). `listened_at` is when the track
// *started*, so ordering by it reflects when each play began. We keep the raw log
// (repeats allowed); "recently played" and "most played" aggregate at read time over
// the `played` rows.
const MIGRATION_011_LISTENS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS listens (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        track_id TEXT NOT NULL,
        player_id TEXT NOT NULL,
        listened_at_unix_seconds INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_listens_at ON listens(listened_at_unix_seconds)",
    "CREATE INDEX IF NOT EXISTS idx_listens_track ON listens(track_id)",
];

// Distinguish confirmed listens from skips. Existing rows predate the completion rule
// (they were recorded on track-start), so they backfill to 'played' — the closest
// truth available. NOTE: the column is added only here via ALTER, never in the v11
// CREATE TABLE, so a fresh DB (which runs both in sequence) doesn't hit a duplicate
// column.
const MIGRATION_023_LISTEN_EVENT_KIND: &[&str] = &[
    "ALTER TABLE listens ADD COLUMN event_kind TEXT NOT NULL DEFAULT 'played'",
    "CREATE INDEX IF NOT EXISTS idx_listens_kind ON listens(event_kind)",
];

// Artist images. Like albums, artists are rebuilt by `save_library` each scan, so the
// served `artwork_url` is re-applied after a rescan from the acquired record. Artist art
// is *acquired-only* (no folder/embedded source) and name-based (the library carries no
// artist MBIDs): `acquired_artist_artwork` mirrors `acquired_album_artwork` — no foreign
// key, `artist_id` stable across rescans, `status` is `acquired` or `not_found`.
const MIGRATION_024_ARTIST_ARTWORK: &[&str] = &[
    "ALTER TABLE artists ADD COLUMN artwork_url TEXT",
    "CREATE TABLE IF NOT EXISTS acquired_artist_artwork (
        artist_id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        remote_url TEXT,
        cache_key TEXT,
        ext TEXT,
        width INTEGER,
        status TEXT NOT NULL,
        acquired_at_unix_seconds INTEGER NOT NULL
    )",
];

// User-curated artist merges: "treat these name variants as one artist". Keyed by the
// NORMALIZED name key (`musicata_core::normalize_artist_key`), not the hash id, so a merge
// survives any change to id derivation. Applied as a server-side regroup that resolves
// each artist's key through this map before computing its id. Reversible (delete a row).
const MIGRATION_025_ARTIST_ALIASES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS artist_aliases (
        alias_key TEXT PRIMARY KEY,
        canonical_key TEXT NOT NULL,
        canonical_name TEXT NOT NULL,
        created_at_unix_seconds INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_artist_aliases_canonical ON artist_aliases(canonical_key)",
];

// Full-text search indexes. These are external-content FTS5 tables (they store
// only the inverted index and read the text from the base tables) kept in sync by
// triggers, so any insert/update/delete on tracks/albums/artists — including future
// incremental writes — is immediately reflected in search with no manual upkeep.
// `remove_diacritics 2` makes matching accent-insensitive ("bjork" finds "Björk").
const MIGRATION_009_FTS: &[&str] = &[
    "CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
        title, artist_name, album_title,
        content='tracks', content_rowid='rowid',
        tokenize='unicode61 remove_diacritics 2'
    )",
    "CREATE TRIGGER IF NOT EXISTS tracks_fts_insert AFTER INSERT ON tracks BEGIN
        INSERT INTO tracks_fts(rowid, title, artist_name, album_title)
        VALUES (new.rowid, new.title, new.artist_name, new.album_title);
    END",
    "CREATE TRIGGER IF NOT EXISTS tracks_fts_delete AFTER DELETE ON tracks BEGIN
        INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title)
        VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_title);
    END",
    "CREATE TRIGGER IF NOT EXISTS tracks_fts_update AFTER UPDATE ON tracks BEGIN
        INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title)
        VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_title);
        INSERT INTO tracks_fts(rowid, title, artist_name, album_title)
        VALUES (new.rowid, new.title, new.artist_name, new.album_title);
    END",
    "INSERT INTO tracks_fts(tracks_fts) VALUES('rebuild')",
    "CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
        title, artist_name,
        content='albums', content_rowid='rowid',
        tokenize='unicode61 remove_diacritics 2'
    )",
    "CREATE TRIGGER IF NOT EXISTS albums_fts_insert AFTER INSERT ON albums BEGIN
        INSERT INTO albums_fts(rowid, title, artist_name)
        VALUES (new.rowid, new.title, new.artist_name);
    END",
    "CREATE TRIGGER IF NOT EXISTS albums_fts_delete AFTER DELETE ON albums BEGIN
        INSERT INTO albums_fts(albums_fts, rowid, title, artist_name)
        VALUES ('delete', old.rowid, old.title, old.artist_name);
    END",
    "CREATE TRIGGER IF NOT EXISTS albums_fts_update AFTER UPDATE ON albums BEGIN
        INSERT INTO albums_fts(albums_fts, rowid, title, artist_name)
        VALUES ('delete', old.rowid, old.title, old.artist_name);
        INSERT INTO albums_fts(rowid, title, artist_name)
        VALUES (new.rowid, new.title, new.artist_name);
    END",
    "INSERT INTO albums_fts(albums_fts) VALUES('rebuild')",
    "CREATE VIRTUAL TABLE IF NOT EXISTS artists_fts USING fts5(
        name,
        content='artists', content_rowid='rowid',
        tokenize='unicode61 remove_diacritics 2'
    )",
    "CREATE TRIGGER IF NOT EXISTS artists_fts_insert AFTER INSERT ON artists BEGIN
        INSERT INTO artists_fts(rowid, name) VALUES (new.rowid, new.name);
    END",
    "CREATE TRIGGER IF NOT EXISTS artists_fts_delete AFTER DELETE ON artists BEGIN
        INSERT INTO artists_fts(artists_fts, rowid, name)
        VALUES ('delete', old.rowid, old.name);
    END",
    "CREATE TRIGGER IF NOT EXISTS artists_fts_update AFTER UPDATE ON artists BEGIN
        INSERT INTO artists_fts(artists_fts, rowid, name)
        VALUES ('delete', old.rowid, old.name);
        INSERT INTO artists_fts(rowid, name) VALUES (new.rowid, new.name);
    END",
    "INSERT INTO artists_fts(artists_fts) VALUES('rebuild')",
];

const MIGRATION_006_METADATA_FIELD_OBSERVATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS track_metadata_field_observations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        observation_id INTEGER NOT NULL,
        track_id TEXT NOT NULL,
        source TEXT NOT NULL,
        field_name TEXT NOT NULL,
        value_json TEXT NOT NULL,
        confidence REAL NOT NULL DEFAULT 0,
        observed_at_unix_seconds INTEGER NOT NULL DEFAULT 0,
        approval_state TEXT NOT NULL DEFAULT 'observed',
        FOREIGN KEY(observation_id) REFERENCES track_metadata_observations(id) ON DELETE CASCADE,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_field_observations_observation_id ON track_metadata_field_observations(observation_id)",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_field_observations_track_id ON track_metadata_field_observations(track_id)",
];

async fn user_version(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query("PRAGMA user_version").fetch_one(pool).await?;
    Ok(row.try_get(0)?)
}

async fn set_user_version(pool: &SqlitePool, version: i64) -> Result<()> {
    sqlx::query(&format!("PRAGMA user_version = {version}"))
        .execute(pool)
        .await?;
    Ok(())
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn path_to_string(path: &PathBuf) -> String {
    path.to_string_lossy().to_string()
}

fn i64_to_usize(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).with_context(|| format!("invalid {field}: {value}"))
}

fn optional_i64_to_u16(value: Option<i64>, field: &str) -> Result<Option<u16>> {
    value
        .map(|value| u16::try_from(value).with_context(|| format!("invalid {field}: {value}")))
        .transpose()
}

fn optional_i64_to_u64(value: Option<i64>, field: &str) -> Result<Option<u64>> {
    value
        .map(|value| u64::try_from(value).with_context(|| format!("invalid {field}: {value}")))
        .transpose()
}

fn f64_to_f32(value: f64, field: &str) -> Result<f32> {
    if value.is_finite() && value >= f32::MIN as f64 && value <= f32::MAX as f64 {
        Ok(value as f32)
    } else {
        Err(anyhow::anyhow!("invalid {field}: {value}"))
    }
}

fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("file size does not fit in SQLite INTEGER")
}

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).context("value does not fit in SQLite INTEGER")
}

fn metadata_values_to_json(values: &[String]) -> Result<Option<String>> {
    if values.is_empty() {
        Ok(None)
    } else {
        serde_json::to_string(values)
            .map(Some)
            .context("failed to serialize metadata values")
    }
}

fn metadata_values_from_json(value: Option<String>, field: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    if value.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&value).with_context(|| format!("invalid {field} metadata values"))
}

fn metadata_field_value_to_json(value: &MetadataFieldValue) -> Result<String> {
    serde_json::to_string(value).context("failed to serialize metadata field value")
}

fn metadata_field_value_from_json(value: String) -> Result<MetadataFieldValue> {
    serde_json::from_str(&value).context("invalid metadata field value")
}

fn approval_state_to_str(state: &MetadataApprovalState) -> &'static str {
    match state {
        MetadataApprovalState::Observed => "observed",
        MetadataApprovalState::Approved => "approved",
        MetadataApprovalState::Rejected => "rejected",
    }
}

fn approval_state_from_str(value: String) -> MetadataApprovalState {
    match value.as_str() {
        "approved" => MetadataApprovalState::Approved,
        "rejected" => MetadataApprovalState::Rejected,
        _ => MetadataApprovalState::Observed,
    }
}

async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    alter_statement: &str,
) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == column)
    });

    if !exists {
        sqlx::query(alter_statement).execute(pool).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ActivityRecord, Database, ListenKind, ResolvedMusicBrainzMetadata, SourceRecord, UserRecord,
    };
    use musicata_core::{
        Album, Artist, BrowseFilter, Library, MetadataApprovalState, MetadataFieldValue,
        ProviderMapping, ScanIssue, Track, TrackMetadataObservation,
    };
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn saves_and_loads_library() {
        let db_path = temp_db_path("roundtrip");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();

        database
            .save_library(&mut library)
            .await
            .expect("save library");
        let loaded = database
            .load_library()
            .await
            .expect("load library")
            .expect("library exists");

        assert_eq!(loaded.provider_id, library.provider_id);
        assert_eq!(loaded.artists.len(), 1);
        assert_eq!(loaded.albums.len(), 1);
        assert_eq!(loaded.tracks.len(), 1);
        assert_eq!(loaded.tracks[0].provider.item_id, "album/song.mp3");
        assert_eq!(loaded.tracks[0].file_size_bytes, Some(1234));
        assert_eq!(loaded.tracks[0].duration_seconds, Some(212.0));
        assert_eq!(
            loaded.tracks[0].modified_at_unix_seconds,
            Some(1_800_000_000)
        );
        assert_eq!(loaded.tracks[0].content_hash, Some("abc123".to_string()));
        assert_eq!(loaded.tracks[0].observed_metadata.len(), 1);
        assert_eq!(loaded.tracks[0].observed_metadata[0].source, "folder_path");
        assert_eq!(loaded.tracks[0].observed_metadata[0].confidence, 0.55);
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].observed_at_unix_seconds,
            1_800_000_000
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].approval_state,
            MetadataApprovalState::Observed
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].title,
            Some("Song".to_string())
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].album_artist_name,
            Some("Album Artist".to_string())
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].recording_date,
            Some("2026-02-03".to_string())
        );
        assert_eq!(loaded.tracks[0].observed_metadata[0].track_total, Some(9));
        assert_eq!(loaded.tracks[0].observed_metadata[0].disc_number, Some(1));
        assert_eq!(loaded.tracks[0].observed_metadata[0].disc_total, Some(2));
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].genres,
            vec!["Dub", "Electronic"]
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].musicbrainz_recording_id,
            Some("recording-id".to_string())
        );
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].embedded_artwork_count,
            1
        );
        let field_observations = &loaded.tracks[0].observed_metadata[0].field_observations;
        assert!(field_observations.iter().any(|field| {
            field.field_name == "title"
                && field.value == MetadataFieldValue::Text("Song".to_string())
                && field.approval_state == MetadataApprovalState::Observed
        }));
        assert!(field_observations.iter().any(|field| {
            field.field_name == "genres"
                && field.value
                    == MetadataFieldValue::TextList(vec![
                        "Dub".to_string(),
                        "Electronic".to_string(),
                    ])
        }));
        assert_eq!(loaded.scan_errors.len(), 1);
        assert_eq!(loaded.scan_errors[0].message, "permission denied");

        let _ = std::fs::remove_file(db_path);
    }

    async fn fixture_library_exists(database: &Database) -> bool {
        database
            .load_library()
            .await
            .expect("load library")
            .is_some()
    }

    #[tokio::test]
    async fn empty_database_has_no_library() {
        let db_path = temp_db_path("empty");
        let database = Database::connect(&db_path).await.expect("connect database");

        assert!(!fixture_library_exists(&database).await);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn preserves_added_at_across_rescans() {
        let db_path = temp_db_path("added-at");
        let database = Database::connect(&db_path).await.expect("connect database");

        // Seed a distinctive original timestamp far from "now".
        let mut library = fixture_library();
        library.tracks[0].added_at_unix_seconds = Some(1_000_000);
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // A fresh rescan carries no in-memory timestamp; the stored one must win.
        let mut rescanned = fixture_library();
        assert!(rescanned.tracks[0].added_at_unix_seconds.is_none());
        database
            .save_library(&mut rescanned)
            .await
            .expect("resave library");

        assert_eq!(rescanned.tracks[0].added_at_unix_seconds, Some(1_000_000));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn search_matches_each_entity_type() {
        let db_path = temp_db_path("search-entities");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        let by_track = database.search("song", 50, 0).await.expect("search");
        assert!(by_track.tracks.iter().any(|track| track.title == "Song"));

        let by_album = database.search("album", 50, 0).await.expect("search");
        assert!(by_album.albums.iter().any(|album| album.title == "Album"));

        let by_artist = database.search("artist", 50, 0).await.expect("search");
        assert!(
            by_artist
                .artists
                .iter()
                .any(|artist| artist.name == "Artist")
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn search_empty_query_returns_no_results() {
        let db_path = temp_db_path("search-empty");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        let results = database.search("   ", 50, 0).await.expect("search");
        assert!(results.artists.is_empty());
        assert!(results.albums.is_empty());
        assert!(results.tracks.is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn search_is_accent_and_case_insensitive() {
        let db_path = temp_db_path("search-accent");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        library.artists[0].name = "Motörhead".to_string();
        library.tracks[0].artist_name = "Motörhead".to_string();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        for query in ["motorhead", "MOTÖRHEAD", "Motorhead"] {
            let results = database.search(query, 50, 0).await.expect("search");
            assert!(
                results
                    .artists
                    .iter()
                    .any(|artist| artist.name == "Motörhead"),
                "query {query:?} should match the accented artist"
            );
        }

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn search_supports_prefix_matching() {
        let db_path = temp_db_path("search-prefix");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // A partial token still matches via prefix, supporting type-ahead.
        let results = database.search("alb", 50, 0).await.expect("search");
        assert!(results.albums.iter().any(|album| album.title == "Album"));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn newly_inserted_track_is_immediately_searchable() {
        let db_path = temp_db_path("search-incremental");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // Insert a track directly, bypassing save_library, to prove the FTS trigger
        // keeps the index fresh for any write path (e.g. future incremental adds).
        sqlx::query(
            "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name,
                album_id, album_title, extension, relative_path, stream_url, path)
             VALUES ('track_zephyr', 'local-disk', 'album/zephyr.mp3', 'Zephyr Anthem', 'artist_1',
                'Artist', 'album_1', 'Album', 'mp3', 'album/zephyr.mp3',
                '/api/tracks/track_zephyr/stream', '/music/album/zephyr.mp3')",
        )
        .execute(&database.pool)
        .await
        .expect("insert track");

        let results = database.search("zephyr", 50, 0).await.expect("search");
        assert!(
            results
                .tracks
                .iter()
                .any(|track| track.title == "Zephyr Anthem"),
            "freshly inserted track should be searchable without a rebuild"
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn radio_stations_round_trip() {
        let db_path = temp_db_path("radio");
        let database = Database::connect(&db_path).await.expect("connect database");

        let id = database
            .create_radio_station(
                "SomaFM",
                "http://ice.somafm.com/groovesalad",
                Some("https://somafm.com"),
                100,
            )
            .await
            .expect("create");
        let stations = database.list_radio_stations().await.expect("list");
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "SomaFM");
        assert_eq!(stations[0].stream_url, "http://ice.somafm.com/groovesalad");

        database
            .update_radio_station(&id, Some("Groove Salad"), None, None)
            .await
            .expect("update");
        let one = database
            .radio_station(&id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(one.name, "Groove Salad");

        database.delete_radio_station(&id).await.expect("delete");
        assert!(database.list_radio_stations().await.unwrap().is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn activities_round_trip() {
        let db_path = temp_db_path("activities");
        let database = Database::connect(&db_path).await.expect("connect database");

        let records = vec![
            ActivityRecord {
                id: 2,
                kind: "scan".to_string(),
                label: "Library rescan".to_string(),
                status: "running".to_string(),
                started_at_unix_seconds: 200,
                finished_at_unix_seconds: None,
                message: None,
            },
            ActivityRecord {
                id: 1,
                kind: "scan".to_string(),
                label: "Initial scan".to_string(),
                status: "ok".to_string(),
                started_at_unix_seconds: 100,
                finished_at_unix_seconds: Some(150),
                message: Some("123 added".to_string()),
            },
        ];
        database.replace_activities(&records).await.expect("save");

        let loaded = database.load_activities().await.expect("load");
        // Newest first (id DESC).
        assert_eq!(loaded, records);

        // Replace wholesale.
        database
            .replace_activities(&records[1..])
            .await
            .expect("replace");
        let loaded = database.load_activities().await.expect("reload");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 1);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn sources_round_trip() {
        let db_path = temp_db_path("sources");
        let database = Database::connect(&db_path).await.expect("connect database");

        let record = SourceRecord {
            id: "smb:nas/music".to_string(),
            kind: "smb".to_string(),
            display_name: "NAS".to_string(),
            enabled: true,
            host: Some("nas.local".to_string()),
            share: Some("music".to_string()),
            base_path: Some("Albums".to_string()),
            username: Some("guest".to_string()),
            password: Some("secret".to_string()),
            domain: None,
            created_at_unix_seconds: 100,
        };
        database.add_source(&record).await.expect("add");

        let sources = database.list_sources().await.expect("list");
        assert_eq!(sources, vec![record.clone()]);

        database
            .set_source_enabled(&record.id, false)
            .await
            .expect("disable");
        let fetched = database
            .source(&record.id)
            .await
            .expect("get")
            .expect("exists");
        assert!(!fetched.enabled);
        assert_eq!(fetched.password.as_deref(), Some("secret"));

        database.delete_source(&record.id).await.expect("delete");
        assert!(database.list_sources().await.unwrap().is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn added_at_preserved_across_resave_per_provider() {
        // Two sources can share the same provider_item_id (a relative path). The
        // added-at preservation must key on (provider_id, item_id), not item id alone.
        let db_path = temp_db_path("added-at-per-provider");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        // Give the fixture track an explicit added-at and a second source's twin.
        library.tracks[0].provider.provider_id = "local-disk".to_string();
        library.tracks[0].provider.item_id = "album/one.mp3".to_string();
        library.tracks[0].added_at_unix_seconds = Some(42);
        let mut twin = library.tracks[0].clone();
        twin.id = "track_twin".to_string();
        twin.provider.provider_id = "smb:nas/music".to_string();
        twin.provider.item_id = "album/one.mp3".to_string();
        twin.added_at_unix_seconds = Some(99);
        library.tracks.push(twin);
        database.save_library(&mut library).await.expect("save");

        // Re-save without explicit timestamps; both must keep their own added-at.
        for track in &mut library.tracks {
            track.added_at_unix_seconds = None;
        }
        database.save_library(&mut library).await.expect("resave");

        let local = database
            .track(&library.tracks[0].id)
            .await
            .expect("get local")
            .expect("exists");
        let smb = database
            .track("track_twin")
            .await
            .expect("get smb")
            .expect("exists");
        assert_eq!(local.added_at_unix_seconds, Some(42));
        assert_eq!(smb.added_at_unix_seconds, Some(99));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn playlists_and_favorites_round_trip() {
        let db_path = temp_db_path("playlists-favorites");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");
        sqlx::query(
            "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name,
                album_id, album_title, extension, relative_path, stream_url, path)
             VALUES ('track_2', 'local-disk', 'album/two.mp3', 'Second', 'artist_1', 'Artist',
                'album_1', 'Album', 'mp3', 'album/two.mp3', '/api/tracks/track_2/stream',
                '/music/album/two.mp3')",
        )
        .execute(&database.pool)
        .await
        .expect("insert second track");

        // Playlists: create with two tracks, in order.
        let id = database
            .create_playlist(
                "Mix",
                Some("notes"),
                &["track_2".into(), "track_1".into()],
                100,
            )
            .await
            .expect("create playlist");
        let playlists = database.list_playlists().await.expect("list");
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "Mix");
        assert_eq!(playlists[0].song_count, 2);
        let tracks = database.playlist_tracks(&id).await.expect("tracks");
        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["track_2", "track_1"]
        );

        // Reorder/replace, then rename.
        database
            .set_playlist_tracks(&id, &["track_1".into()], 200)
            .await
            .expect("set tracks");
        database
            .update_playlist_meta(&id, Some("Renamed"), None, 200)
            .await
            .expect("rename");
        let detail = database.playlist(&id).await.expect("get").expect("exists");
        assert_eq!(detail.name, "Renamed");
        assert_eq!(detail.song_count, 1);

        database.delete_playlist(&id).await.expect("delete");
        assert!(database.list_playlists().await.unwrap().is_empty());

        // Favorites: star a track, album, and artist.
        database.set_favorite("track", "track_1", 1).await.unwrap();
        database.set_favorite("track", "track_1", 1).await.unwrap(); // idempotent
        database.set_favorite("album", "album_1", 2).await.unwrap();
        database
            .set_favorite("artist", "artist_1", 3)
            .await
            .unwrap();
        assert_eq!(database.starred_tracks().await.unwrap().len(), 1);
        assert_eq!(database.starred_albums().await.unwrap().len(), 1);
        assert_eq!(database.starred_artists().await.unwrap().len(), 1);
        assert_eq!(database.favorite_ids("track").await.unwrap(), ["track_1"]);

        database.clear_favorite("track", "track_1").await.unwrap();
        assert!(database.starred_tracks().await.unwrap().is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn listening_history_aggregates_recent_and_most_played() {
        let db_path = temp_db_path("listening-history");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // A second track so ordering is observable.
        sqlx::query(
            "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name,
                album_id, album_title, extension, relative_path, stream_url, path)
             VALUES ('track_2', 'local-disk', 'album/two.mp3', 'Second', 'artist_1', 'Artist',
                'album_1', 'Album', 'mp3', 'album/two.mp3', '/api/tracks/track_2/stream',
                '/music/album/two.mp3')",
        )
        .execute(&database.pool)
        .await
        .expect("insert second track");

        // track_1 played twice, track_2 once. Newest listen is track_1 at t=300.
        let played = ListenKind::Played;
        database.record_listen("track_1", "p", 100, played).await.unwrap();
        database.record_listen("track_2", "p", 200, played).await.unwrap();
        database.record_listen("track_1", "p", 300, played).await.unwrap();

        // Recently played: distinct tracks, newest-listen first, each with its most
        // recent listen time (track_1's is 300, not 100).
        let recent = database.recently_played(10).await.expect("recent");
        let recent_ids: Vec<_> = recent.iter().map(|(track, _)| track.id.as_str()).collect();
        assert_eq!(recent_ids, ["track_1", "track_2"]);
        assert_eq!(recent[0].1, 300);
        assert_eq!(recent[1].1, 200);

        // Most played: by listen count, track_1 (2) before track_2 (1).
        let most = database.most_played(10).await.expect("most");
        assert_eq!(most[0].0.id, "track_1");
        assert_eq!(most[0].1, 2);
        assert_eq!(most[1].0.id, "track_2");
        assert_eq!(most[1].1, 1);

        // A listen for a track no longer in the library is dropped by the join.
        database.record_listen("ghost", "p", 400, played).await.unwrap();
        let recent = database.recently_played(10).await.expect("recent again");
        assert!(recent.iter().all(|(track, _)| track.id != "ghost"));

        // Retention: pruning drops listens before the cutoff. Removing everything
        // before t=300 leaves only track_1's t=300 listen (and the ghost at t=400).
        let removed = database.prune_listens_before(300).await.expect("prune");
        assert_eq!(removed, 2); // track_1@100 and track_2@200
        let most = database.most_played(10).await.expect("most after prune");
        assert_eq!(most.len(), 1);
        assert_eq!(most[0].0.id, "track_1");
        assert_eq!(most[0].1, 1);

        // The user-facing "clear history" wipes every remaining listen.
        let cleared = database.clear_listens().await.expect("clear");
        assert_eq!(cleared, 2); // track_1@300 and the ghost@400
        assert!(database.most_played(10).await.expect("most after clear").is_empty());
        assert!(database.clear_listens().await.expect("clear again") == 0);

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn streak_days_counts_current_and_longest() {
        // Active days 96, 98, 99, 100 with today = 100: the current streak is the
        // 98-99-100 run (3); 96 is isolated. Longest is also 3.
        assert_eq!(super::streak_days(&[96, 98, 99, 100], 100), (3, 3));
        // The latest active day is yesterday (99) — still a live streak counted from 99.
        assert_eq!(super::streak_days(&[97, 98, 99], 100), (3, 3));
        // Latest active day is older than yesterday: the streak is broken (0), but the
        // 90-91 run is still the longest ever (2).
        assert_eq!(super::streak_days(&[90, 91, 95], 100), (0, 2));
        assert_eq!(super::streak_days(&[], 100), (0, 0));
        assert_eq!(super::streak_days(&[100], 100), (1, 1));
    }

    #[test]
    fn listening_sessions_group_by_gap() {
        let gap = 1800;
        // Two bursts within 30 min, far apart from each other: 2 sessions, longest 3.
        assert_eq!(
            super::listening_sessions(&[0, 300, 600, 10_000, 10_100], gap),
            (2, 3)
        );
        // Every play more than the gap apart: each is its own one-play session.
        assert_eq!(super::listening_sessions(&[0, 5_000, 10_000], gap), (3, 1));
        assert_eq!(super::listening_sessions(&[], gap), (0, 0));
        assert_eq!(super::listening_sessions(&[42], gap), (1, 1));
    }

    #[tokio::test]
    async fn listening_stats_aggregates_streaks_sessions_and_favorites() {
        const DAY: i64 = 86_400;
        let db_path = temp_db_path("listening-stats");
        let database = Database::connect(&db_path).await.expect("connect database");

        for (id, title) in [("t_a", "Alpha"), ("t_b", "Beta")] {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds)
                 VALUES (?1, 'local-disk', ?2, ?3, 'ar_1', 'Artist', 'al_1', 'Album',
                    'mp3', ?2, ?4, ?5, 10)",
            )
            .bind(id)
            .bind(format!("album/{id}.mp3"))
            .bind(title)
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/music/album/{id}.mp3"))
            .execute(&database.pool)
            .await
            .expect("insert track");
        }

        let now = 100 * DAY + 50_000; // sometime on day index 100
        let played = ListenKind::Played;
        let skipped = ListenKind::Skipped;
        // Plays on days 100 (x2, one session), 99, 98 (a 3-day current streak), and an
        // isolated day 96. One skip on day 97 (excluded from plays + streak).
        database.record_listen("t_a", "p", 100 * DAY + 1_000, played).await.unwrap();
        database.record_listen("t_a", "p", 100 * DAY + 1_300, played).await.unwrap();
        database.record_listen("t_a", "p", 99 * DAY + 500, played).await.unwrap();
        database.record_listen("t_a", "p", 98 * DAY + 500, played).await.unwrap();
        database.record_listen("t_b", "p", 96 * DAY + 500, played).await.unwrap();
        database.record_listen("t_b", "p", 97 * DAY + 500, skipped).await.unwrap();

        database.set_favorite("track", "t_a", 1).await.unwrap();
        database.set_favorite("track", "t_b", 2).await.unwrap();
        database.set_favorite("album", "al_1", 3).await.unwrap();
        database.set_favorite("artist", "ar_1", 4).await.unwrap();

        let stats = database.listening_stats(now).await.expect("stats");
        assert_eq!(stats.total_plays, 5);
        assert_eq!(stats.total_skips, 1);
        assert_eq!(stats.distinct_tracks_played, 2);
        assert_eq!(stats.plays_last_7_days, 5);
        assert_eq!(stats.plays_last_30_days, 5);
        assert_eq!(stats.current_streak_days, 3); // days 98-99-100
        assert_eq!(stats.longest_streak_days, 3);
        assert_eq!(stats.listening_sessions, 4); // 96 | 98 | 99 | 100(x2)
        assert_eq!(stats.longest_session_plays, 2);
        assert_eq!(stats.favorite_tracks, 2);
        assert_eq!(stats.favorite_albums, 1);
        assert_eq!(stats.favorite_artists, 1);

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn album_loudness_weights_by_duration() {
        // Equal weights: -10 LUFS + -20 LUFS → linear mean (0.1+0.01)/2 = 0.055 →
        // 10·log10(0.055) ≈ -12.596. The album peak is the max of the track peaks.
        let (lufs, peak) = super::album_loudness_from_tracks(&[
            (-10.0, Some(100.0), Some(-1.5)),
            (-20.0, Some(100.0), Some(-2.0)),
        ]);
        assert!((lufs.unwrap() - (-12.596)).abs() < 0.01, "{lufs:?}");
        assert_eq!(peak, Some(-1.5));

        // A long quiet track dominates the duration-weighted mean.
        let (weighted, _) = super::album_loudness_from_tracks(&[
            (-10.0, Some(10.0), None),
            (-20.0, Some(1000.0), None),
        ]);
        assert!(weighted.unwrap() < -18.0, "{weighted:?}");

        assert_eq!(super::album_loudness_from_tracks(&[]), (None, None));
    }

    #[tokio::test]
    async fn album_loudness_recomputed_from_track_loudness() {
        let db_path = temp_db_path("album-loudness");
        let database = Database::connect(&db_path).await.expect("connect database");

        for (id, album, duration) in
            [("t1", "al_1", 100.0), ("t2", "al_1", 100.0), ("t3", "al_2", 60.0)]
        {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds, duration_seconds)
                 VALUES (?1, 'local-disk', ?2, ?1, 'ar', 'Artist', ?3, 'Album', 'mp3', ?2,
                    ?4, ?5, 10, ?6)",
            )
            .bind(id)
            .bind(format!("a/{id}.mp3"))
            .bind(album)
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/m/{id}.mp3"))
            .bind(duration)
            .execute(&database.pool)
            .await
            .expect("insert track");
        }
        database.upsert_track_loudness("t1", Some(-10.0), Some(-1.5), 1).await.unwrap();
        database.upsert_track_loudness("t2", Some(-20.0), Some(-2.0), 1).await.unwrap();
        database.upsert_track_loudness("t3", Some(-14.0), Some(-1.0), 1).await.unwrap();

        let written = database.recompute_album_loudness(2).await.expect("recompute");
        assert_eq!(written, 2);

        let (lufs, peak) = database.album_loudness("al_1").await.unwrap().expect("al_1");
        assert!((lufs - (-12.596)).abs() < 0.01, "{lufs}");
        assert_eq!(peak, -1.5);

        // A single-track album equals that track's loudness.
        let (lufs2, _) = database.album_loudness("al_2").await.unwrap().expect("al_2");
        assert!((lufs2 - (-14.0)).abs() < 0.001, "{lufs2}");

        assert!(database.album_loudness("missing").await.unwrap().is_none());
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn sqlite_vec_extension_is_loaded() {
        use sqlx::Row;
        let db_path = temp_db_path("vec-loaded");
        let database = Database::connect(&db_path).await.expect("connect database");

        // A vec0 virtual table only exists if the sqlite-vec extension loaded on this connection.
        sqlx::query("CREATE VIRTUAL TABLE vtest USING vec0(embedding float[3])")
            .execute(&database.pool)
            .await
            .expect("create vec0 table (sqlite-vec not loaded?)");
        for (id, vector) in [(1_i64, "[1,0,0]"), (2, "[0,1,0]"), (3, "[0.9,0.1,0]")] {
            sqlx::query("INSERT INTO vtest(rowid, embedding) VALUES (?1, ?2)")
                .bind(id)
                .bind(vector)
                .execute(&database.pool)
                .await
                .expect("insert vector");
        }
        // KNN: the two nearest to [1,0,0] are rowid 1 (itself) then 3 (close), not 2 (orthogonal).
        let rows = sqlx::query(
            "SELECT rowid FROM vtest WHERE embedding MATCH ?1 ORDER BY distance LIMIT 2",
        )
        .bind("[1,0,0]")
        .fetch_all(&database.pool)
        .await
        .expect("knn query");
        let ids: Vec<i64> = rows
            .iter()
            .map(|row| row.try_get::<i64, _>("rowid").unwrap())
            .collect();
        assert_eq!(ids, vec![1, 3]);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn audio_embedding_store_and_similarity() {
        let db_path = temp_db_path("embedding-sim");
        let database = Database::connect(&db_path).await.expect("connect database");

        // 2048-d vectors (the model dim): a and c point similarly; b is orthogonal.
        let dim = super::AUDIO_EMBEDDING_DIM;
        let mut a = vec![0.0f32; dim];
        a[0] = 1.0;
        let mut b = vec![0.0f32; dim];
        b[1] = 1.0;
        let mut c = vec![0.0f32; dim];
        c[0] = 0.9;
        c[1] = 0.1;

        database.upsert_track_embedding("a", &a, "panns", Some("1"), "[]", 1).await.unwrap();
        database.upsert_track_embedding("b", &b, "panns", None, "[]", 1).await.unwrap();
        database.upsert_track_embedding("c", &c, "panns", None, "[]", 1).await.unwrap();
        assert_eq!(database.embedding_count().await.unwrap(), 3);

        // Nearest to a (excluding itself): c (similar) before b (orthogonal).
        let similar = database.similar_by_embedding("a", 2).await.unwrap();
        let ids: Vec<&str> = similar.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b"]);

        // Re-analyzing replaces, doesn't duplicate.
        database.upsert_track_embedding("a", &a, "panns", None, "[\"Music\"]", 2).await.unwrap();
        assert_eq!(database.embedding_count().await.unwrap(), 3);

        // A track with no embedding yields nothing.
        assert!(database.similar_by_embedding("missing", 5).await.unwrap().is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn diversify_interleaves_and_caps_artists() {
        let ranked: Vec<(String, String)> = [
            ("a1", "A"), ("a2", "A"), ("a3", "A"), ("b1", "B"), ("b2", "B"), ("b3", "B"),
            ("c1", "C"), ("c2", "C"),
        ]
        .iter()
        .map(|(id, artist)| (id.to_string(), artist.to_string()))
        .collect();
        let artist_of = |id: &str| ranked.iter().find(|(i, _)| i == id).unwrap().1.clone();

        // 3 artists, want 6 → ~2 each, interleaved (not a1,a2,a3,…).
        let radio = super::diversify(&ranked, 6);
        assert_eq!(radio.len(), 6);
        for window in radio.windows(2) {
            assert_ne!(artist_of(&window[0]), artist_of(&window[1]), "back-to-back: {radio:?}");
        }
        let first_three: std::collections::HashSet<_> =
            radio[..3].iter().map(|id| artist_of(id)).collect();
        assert_eq!(first_three.len(), 3, "first three should be three distinct artists");
    }

    #[test]
    fn diversify_fills_station_when_few_artists() {
        // Only 2 artists but a 4-track station → 2 each, alternating.
        let ranked: Vec<(String, String)> = [("a1", "A"), ("a2", "A"), ("a3", "A"), ("b1", "B"), ("b2", "B")]
            .iter()
            .map(|(id, artist)| (id.to_string(), artist.to_string()))
            .collect();
        let radio = super::diversify(&ranked, 4);
        assert_eq!(radio, vec!["a1", "b1", "a2", "b2"]);
    }

    #[tokio::test]
    async fn audio_radio_interleaves_artists() {
        let db_path = temp_db_path("audio-radio");
        let database = Database::connect(&db_path).await.expect("connect database");
        let dim = super::AUDIO_EMBEDDING_DIM;
        let vector = |x: f32, y: f32| {
            let mut v = vec![0.0f32; dim];
            v[0] = x;
            v[1] = y;
            v
        };
        // A seed plus 3 tracks by artist A (acoustically nearest) and 2 by artist B.
        let data = [
            ("s", "Seed", 1.0, 0.0),
            ("a1", "A", 0.99, 0.01),
            ("a2", "A", 0.98, 0.02),
            ("a3", "A", 0.97, 0.03),
            ("b1", "B", 0.90, 0.10),
            ("b2", "B", 0.88, 0.12),
        ];
        for (id, artist, _, _) in data {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds)
                 VALUES (?1, 'local-disk', ?2, ?1, ?3, ?3, 'al', 'Album', 'mp3', ?2, ?4, ?5, 10)",
            )
            .bind(id)
            .bind(format!("a/{id}.mp3"))
            .bind(artist)
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/m/{id}.mp3"))
            .execute(&database.pool)
            .await
            .expect("insert track");
        }
        for (id, _, x, y) in data {
            database.upsert_track_embedding(id, &vector(x, y), "m", None, "[]", 1).await.unwrap();
        }

        // The raw KNN would be a1,a2,a3,b1 (all A first); the radio interleaves A and B.
        let radio = database.audio_radio("s", 4).await.unwrap();
        assert_eq!(radio.len(), 4);
        let artist_of = |id: &str| data.iter().find(|(i, ..)| *i == id).unwrap().1;
        for window in radio.windows(2) {
            assert_ne!(artist_of(&window[0]), artist_of(&window[1]), "back-to-back: {radio:?}");
        }
        let artists: std::collections::HashSet<_> = radio.iter().map(|id| artist_of(id)).collect();
        assert!(artists.contains(&"A") && artists.contains(&"B"), "both artists present");

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn listens_distinguish_plays_from_skips() {
        let db_path = temp_db_path("listens-event-kind");
        let database = Database::connect(&db_path).await.expect("connect database");

        // Two bare tracks, inserted directly (no fixture) so never_played's whole-table
        // scan is predictable.
        for (id, title) in [("t_a", "Alpha"), ("t_b", "Beta")] {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds)
                 VALUES (?1, 'local-disk', ?2, ?3, 'artist_1', 'Artist', 'album_1', 'Album',
                    'mp3', ?2, ?4, ?5, 10)",
            )
            .bind(id)
            .bind(format!("album/{id}.mp3"))
            .bind(title)
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/music/album/{id}.mp3"))
            .execute(&database.pool)
            .await
            .expect("insert track");
        }

        let played = ListenKind::Played;
        let skipped = ListenKind::Skipped;
        // t_a: a real listen at t=100 and t=200. t_b: only skips at t=150 and t=250.
        database.record_listen("t_a", "p", 100, played).await.unwrap();
        database.record_listen("t_a", "p", 200, played).await.unwrap();
        database.record_listen("t_b", "p", 150, skipped).await.unwrap();
        database.record_listen("t_b", "p", 250, skipped).await.unwrap();

        // most_played counts confirmed listens only — t_b's skips don't appear.
        let most = database.most_played(10).await.unwrap();
        assert_eq!(most.len(), 1);
        assert_eq!(most[0].0.id, "t_a");
        assert_eq!(most[0].1, 2);

        // most_skipped is the mirror over skipped rows.
        let skips = database.most_skipped(10).await.unwrap();
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].0.id, "t_b");
        assert_eq!(skips[0].1, 2);

        // never_played = no confirmed listen. t_b qualifies (only skipped); t_a doesn't.
        let never: Vec<_> = database
            .never_played(10)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(never.contains(&"t_b".to_string()));
        assert!(!never.contains(&"t_a".to_string()));

        // most_played_since(150) drops t_a's t=100 listen, keeping only t=200.
        let since = database.most_played_since(10, 150).await.unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].0.id, "t_a");
        assert_eq!(since[0].1, 1);

        // Forgotten favorites: star both. With a cutoff of 300, neither has a confirmed
        // listen at/after it, so both are "forgotten".
        database.set_favorite("track", "t_a", 50).await.unwrap();
        database.set_favorite("track", "t_b", 60).await.unwrap();
        let forgotten: Vec<_> = database
            .forgotten_favorites(10, 300)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(forgotten.contains(&"t_a".to_string()));
        assert!(forgotten.contains(&"t_b".to_string()));

        // A fresh confirmed listen at t=400 takes t_a off the forgotten list.
        database.record_listen("t_a", "p", 400, played).await.unwrap();
        let forgotten: Vec<_> = database
            .forgotten_favorites(10, 300)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(forgotten, ["t_b"]);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn frequently_skipped_needs_more_skips_than_plays_and_a_min_sample() {
        let db_path = temp_db_path("frequently-skipped");
        let database = Database::connect(&db_path).await.expect("connect database");

        for id in ["t_skip", "t_ok", "t_one", "t_tie"] {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds)
                 VALUES (?1, 'local-disk', ?1, ?1, 'artist_1', 'Artist', 'album_1', 'Album',
                    'mp3', ?2, ?3, ?4, 10)",
            )
            .bind(id)
            .bind(format!("album/{id}.mp3"))
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/music/album/{id}.mp3"))
            .execute(&database.pool)
            .await
            .expect("insert track");
        }

        let (played, skipped) = (ListenKind::Played, ListenKind::Skipped);
        // t_skip: 3 skips vs 1 play → penalized (skips > plays, >= 2 skips).
        for t in [10, 20, 30] {
            database.record_listen("t_skip", "p", t, skipped).await.unwrap();
        }
        database.record_listen("t_skip", "p", 40, played).await.unwrap();
        // t_ok: 1 skip vs 3 plays → not penalized.
        database.record_listen("t_ok", "p", 10, skipped).await.unwrap();
        for t in [20, 30, 40] {
            database.record_listen("t_ok", "p", t, played).await.unwrap();
        }
        // t_one: a single skip, no plays → skips > plays but below the 2-skip floor.
        database.record_listen("t_one", "p", 10, skipped).await.unwrap();
        // t_tie: 2 skips vs 2 plays → not strictly more skips.
        for t in [10, 20] {
            database.record_listen("t_tie", "p", t, skipped).await.unwrap();
        }
        for t in [30, 40] {
            database.record_listen("t_tie", "p", t, played).await.unwrap();
        }

        let penalized = database.frequently_skipped_track_ids().await.unwrap();
        assert_eq!(penalized.len(), 1, "only t_skip qualifies: {penalized:?}");
        assert!(penalized.contains("t_skip"));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn users_and_sessions_round_trip() {
        let db_path = temp_db_path("users-sessions");
        let database = Database::connect(&db_path).await.expect("connect database");

        assert_eq!(database.count_users().await.unwrap(), 0, "starts in setup mode");

        let admin = UserRecord {
            id: "u1".into(),
            username: "alice".into(),
            password_hash: "argon2-hash".into(),
            role: "admin".into(),
            api_token: "tok-abc".into(),
            created_at_unix_seconds: 100,
        };
        database.create_user(&admin).await.unwrap();
        assert_eq!(database.count_users().await.unwrap(), 1);
        assert_eq!(database.user_by_username("alice").await.unwrap().unwrap().role, "admin");
        assert_eq!(database.user_by_api_token("tok-abc").await.unwrap().unwrap().id, "u1");
        assert!(database.user_by_api_token("nope").await.unwrap().is_none());

        // Session lookup respects expiry.
        database.create_session("hash1", "u1", 100, 1000, Some("phone")).await.unwrap();
        assert_eq!(database.session_user("hash1", 500).await.unwrap().unwrap().username, "alice");
        assert!(database.session_user("hash1", 2000).await.unwrap().is_none(), "expired");
        assert!(database.session_user("missing", 500).await.unwrap().is_none());

        // Pruning drops only expired rows.
        database.create_session("hash2", "u1", 100, 5000, None).await.unwrap();
        assert_eq!(database.prune_expired_sessions(2000).await.unwrap(), 1);
        assert!(database.session_user("hash2", 2000).await.unwrap().is_some());

        // Updates + deletion (which also clears sessions).
        database.set_user_role("u1", "listener").await.unwrap();
        database.set_user_api_token("u1", "tok-xyz").await.unwrap();
        assert!(database.user_by_api_token("tok-abc").await.unwrap().is_none());
        assert_eq!(database.user_by_username("alice").await.unwrap().unwrap().role, "listener");
        database.delete_user("u1").await.unwrap();
        assert_eq!(database.count_users().await.unwrap(), 0);
        assert!(database.session_user("hash2", 1000).await.unwrap().is_none(), "sessions cascade");

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn browse_index_reports_facets_from_sql() {
        let db_path = temp_db_path("browse-index-sql");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        let index = database.browse_index().await.expect("browse index");
        // fixture observation: genres Dub/Electronic, composers Composer, year 2026,
        // relative_path "album/song.mp3" -> folder "album".
        assert!(
            index
                .genres
                .iter()
                .any(|facet| facet.value == "Dub" && facet.track_count == 1)
        );
        assert!(
            index
                .composers
                .iter()
                .any(|facet| facet.value == "Composer" && facet.track_count == 1)
        );
        assert!(
            index
                .years
                .iter()
                .any(|facet| facet.value == 2026 && facet.track_count == 1)
        );
        assert!(
            index
                .folders
                .iter()
                .any(|facet| facet.value == "album" && facet.track_count == 1)
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn list_tracks_filters_and_paginates_in_sql() {
        let db_path = temp_db_path("list-tracks-sql");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // Genre filter matches the fixture track via its observation's JSON array.
        let filter = BrowseFilter {
            genre: Some("dub".to_string()),
            ..BrowseFilter::default()
        };
        let (tracks, total) = database
            .list_tracks(&filter, None, -1, 0)
            .await
            .expect("list tracks");
        assert_eq!(total, 1);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Song");
        // Listed tracks omit observation provenance.
        assert!(tracks[0].observed_metadata.is_empty());

        // A non-matching genre returns nothing.
        let filter = BrowseFilter {
            genre: Some("jazz".to_string()),
            ..BrowseFilter::default()
        };
        let (tracks, total) = database
            .list_tracks(&filter, None, -1, 0)
            .await
            .expect("list tracks");
        assert_eq!(total, 0);
        assert!(tracks.is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn list_albums_filtered_narrows_to_albums_with_matching_tracks() {
        let db_path = temp_db_path("albums-filtered");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // The fixture's one track is genre "Dub", so its album is in the dub result.
        let dub = BrowseFilter {
            genre: Some("dub".to_string()),
            ..BrowseFilter::default()
        };
        let (albums, total) = database
            .list_albums_filtered(&dub, None, -1, 0)
            .await
            .expect("list albums");
        assert_eq!(total, 1);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].id, library.albums[0].id);

        // A genre no track has yields no albums (not the whole library).
        let jazz = BrowseFilter {
            genre: Some("jazz".to_string()),
            ..BrowseFilter::default()
        };
        let (albums, total) = database
            .list_albums_filtered(&jazz, None, -1, 0)
            .await
            .expect("list albums");
        assert_eq!(total, 0);
        assert!(albums.is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn scrobble_queue_roundtrips() {
        let db_path = temp_db_path("scrobble-queue");
        let database = Database::connect(&db_path).await.expect("connect database");

        assert!(database.pending_scrobbles(10).await.unwrap().is_empty());
        database.enqueue_scrobble("t1", 100).await.unwrap();
        database.enqueue_scrobble("t2", 200).await.unwrap();

        // Oldest-first, carrying track id + listened-at.
        let pending = database.pending_scrobbles(10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!((pending[0].1.as_str(), pending[0].2), ("t1", 100));
        assert_eq!((pending[1].1.as_str(), pending[1].2), ("t2", 200));

        // Deleting the submitted row drains it.
        database.delete_scrobble(pending[0].0).await.unwrap();
        let pending = database.pending_scrobbles(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "t2");

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn tag_facet_and_filter_match_audio_tags() {
        let db_path = temp_db_path("tag-facet");
        let database = Database::connect(&db_path).await.expect("connect database");
        let dim = super::AUDIO_EMBEDDING_DIM;
        let embedding = vec![0.0f32; dim];
        // Two tracks with overlapping musicata-ml tags (objects, as production stores them).
        let data = [
            (
                "t1",
                r#"[{"label":"Rock music","score":0.8},{"label":"Music","score":0.95}]"#,
            ),
            (
                "t2",
                r#"[{"label":"Jazz","score":0.7},{"label":"Music","score":0.9}]"#,
            ),
        ];
        for (id, tags) in data {
            sqlx::query(
                "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id,
                    artist_name, album_id, album_title, extension, relative_path, stream_url,
                    path, added_at_unix_seconds)
                 VALUES (?1, 'local-disk', ?2, ?1, 'ar', 'Artist', 'al', 'Album', 'mp3', ?2, ?3, ?4, 10)",
            )
            .bind(id)
            .bind(format!("a/{id}.mp3"))
            .bind(format!("/api/tracks/{id}/stream"))
            .bind(format!("/m/{id}.mp3"))
            .execute(&database.pool)
            .await
            .expect("insert track");
            database
                .upsert_track_embedding(id, &embedding, "m", None, tags, 1)
                .await
                .unwrap();
        }

        // Facet: most-common tag first; "Music" on both, the genres on one each.
        let tags = database.browse_index().await.unwrap().tags;
        let counts: std::collections::HashMap<_, _> =
            tags.iter().map(|t| (t.value.as_str(), t.track_count)).collect();
        assert_eq!(counts.get("Music"), Some(&2));
        assert_eq!(counts.get("Rock music"), Some(&1));
        assert_eq!(counts.get("Jazz"), Some(&1));
        assert_eq!(tags.first().map(|t| t.value.as_str()), Some("Music")); // count-desc ordering

        // Filter: a tag narrows the track list (case-insensitive); an absent tag returns nothing.
        let filtered = |tag: &str| {
            let f = BrowseFilter {
                tag: Some(tag.to_string()),
                ..BrowseFilter::default()
            };
            let database = &database;
            async move { database.list_tracks(&f, None, -1, 0).await.unwrap() }
        };
        let (rock, total) = filtered("rock music").await;
        assert_eq!(total, 1);
        assert_eq!(rock[0].id, "t1");
        let (music, total) = filtered("Music").await;
        assert_eq!(total, 2);
        assert_eq!(music.len(), 2);
        let (_, total) = filtered("Reggae").await;
        assert_eq!(total, 0);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn persists_players_and_zones() {
        let db_path = temp_db_path("players-zones");
        let database = Database::connect(&db_path).await.expect("connect database");

        database
            .insert_zone("zone-a", "Zone A")
            .await
            .expect("zone");
        assert_eq!(database.list_zones().await.expect("zones").len(), 1);

        let record = super::PlayerRecord {
            id: "mpd-x".to_string(),
            kind: "mpd".to_string(),
            address: "127.0.0.1:6600".to_string(),
            name: "Test".to_string(),
            zone_id: None,
        };
        database.upsert_player(&record).await.expect("upsert");
        assert_eq!(
            database
                .player_record("mpd-x")
                .await
                .expect("record")
                .expect("some")
                .name,
            "Test"
        );

        database
            .update_player_name("mpd-x", "Renamed")
            .await
            .expect("rename");
        database
            .update_player_zone("mpd-x", Some("zone-a"))
            .await
            .expect("zone assign");
        let in_zone = database.players_in_zone("zone-a").await.expect("in zone");
        assert_eq!(in_zone.len(), 1);
        assert_eq!(in_zone[0].name, "Renamed");

        // Give the zone a persisted queue so we can assert it's cleaned up too.
        database
            .save_zone_queue(
                "zone-a",
                &super::PlayerPlayback {
                    status: musicata_core::PlaybackStatus::Paused,
                    position: Some(0),
                    elapsed_seconds: Some(0.0),
                    volume: None,
                    repeat: musicata_core::RepeatMode::Off,
                    shuffle: false,
                },
                &[musicata_core::QueueItem {
                    track_id: Some("t1".to_string()),
                    title: "One".to_string(),
                    ..Default::default()
                }],
            )
            .await
            .expect("save zone queue");
        assert!(
            database
                .load_zone_queue("zone-a")
                .await
                .expect("load")
                .is_some()
        );

        // Deleting the zone clears the player's zone (FK ON DELETE SET NULL) and
        // drops the zone's persisted queue.
        database.delete_zone("zone-a").await.expect("delete zone");
        assert_eq!(
            database
                .player_record("mpd-x")
                .await
                .expect("record")
                .expect("some")
                .zone_id,
            None
        );
        assert!(
            database
                .load_zone_queue("zone-a")
                .await
                .expect("load")
                .is_none(),
            "zone queue dropped with the zone"
        );

        database
            .delete_player("mpd-x")
            .await
            .expect("delete player");
        assert!(database.list_players().await.expect("players").is_empty());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn rescan_with_only_new_observation_timestamps_reports_no_changes() {
        let db_path = temp_db_path("idempotent-rescan");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        // Simulate a later scan of unchanged files: identical content, but every
        // observation (and its field observations) carries a fresh scan timestamp.
        let mut rescanned = library.clone();
        for track in &mut rescanned.tracks {
            for observation in &mut track.observed_metadata {
                observation.observed_at_unix_seconds += 86_400;
                observation.field_observations = observation.effective_field_observations();
                for field in &mut observation.field_observations {
                    field.observed_at_unix_seconds += 86_400;
                }
            }
        }

        let changes = database
            .detect_library_changes(&rescanned)
            .await
            .expect("detect changes");
        assert_eq!(changes.added, 0);
        assert_eq!(changes.removed, 0);
        assert_eq!(changes.modified, 0);
        assert!(!changes.has_changes());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn detects_added_removed_and_modified_tracks() {
        let db_path = temp_db_path("changes");
        let database = Database::connect(&db_path).await.expect("connect database");
        let mut library = fixture_library();
        database
            .save_library(&mut library)
            .await
            .expect("save library");

        let mut metadata_changed = library.clone();
        metadata_changed.tracks[0].observed_metadata[0].title = Some("Retagged".to_string());
        let changes = database
            .detect_library_changes(&metadata_changed)
            .await
            .expect("detect changes");
        assert_eq!(changes.added, 0);
        assert_eq!(changes.removed, 0);
        assert_eq!(changes.modified, 1);

        let mut review_changed = library.clone();
        review_changed.tracks[0].observed_metadata[0].field_observations =
            review_changed.tracks[0].observed_metadata[0].effective_field_observations();
        review_changed.tracks[0].observed_metadata[0].field_observations[0].approval_state =
            MetadataApprovalState::Approved;
        let changes = database
            .detect_library_changes(&review_changed)
            .await
            .expect("detect changes");
        assert_eq!(changes.added, 0);
        assert_eq!(changes.removed, 0);
        assert_eq!(changes.modified, 1);

        let mut changed = library.clone();
        changed.tracks[0].content_hash = Some("def456".to_string());
        changed.tracks.push(Track {
            id: "track_2".to_string(),
            provider: ProviderMapping {
                provider_id: "local-disk".to_string(),
                item_id: "album/new-song.mp3".to_string(),
            },
            observed_metadata: vec![fixture_observation("New Song", 2)],
            title: "New Song".to_string(),
            artist_id: "artist_1".to_string(),
            artist_name: "Artist".to_string(),
            album_id: "album_1".to_string(),
            album_title: "Album".to_string(),
            year: Some(2026),
            track_number: Some(2),
            disc_number: None,
            extension: "mp3".to_string(),
            file_size_bytes: Some(10),
            duration_seconds: None,
            modified_at_unix_seconds: Some(1_800_000_100),
            content_hash: Some("new789".to_string()),
            relative_path: "album/new-song.mp3".to_string(),
            stream_url: "/api/tracks/track_2/stream".to_string(),
            added_at_unix_seconds: None,
            path: PathBuf::from("/music/album/new-song.mp3"),
        });

        let changes = database
            .detect_library_changes(&changed)
            .await
            .expect("detect changes");
        assert_eq!(changes.added, 1);
        assert_eq!(changes.removed, 0);
        assert_eq!(changes.modified, 1);
        assert!(changes.has_changes());

        changed.tracks.remove(0);
        let changes = database
            .detect_library_changes(&changed)
            .await
            .expect("detect changes");
        assert_eq!(changes.added, 1);
        assert_eq!(changes.removed, 1);
        assert_eq!(changes.modified, 0);

        let _ = std::fs::remove_file(db_path);
    }

    fn fixture_library() -> Library {
        Library {
            provider_id: "local-disk".to_string(),
            source_root: "/music".to_string(),
            artists: vec![Artist {
                id: "artist_1".to_string(),
                name: "Artist".to_string(),
                album_count: 1,
                track_count: 1,
                artwork_url: None,
            }],
            albums: vec![Album {
                id: "album_1".to_string(),
                title: "Album".to_string(),
                artist_id: "artist_1".to_string(),
                artist_name: "Artist".to_string(),
                year: Some(2026),
                track_count: 1,
                artwork_url: Some("/api/albums/album_1/artwork".to_string()),
                artwork_path: Some(PathBuf::from("/music/album/cover.jpg")),
            }],
            tracks: vec![Track {
                id: "track_1".to_string(),
                provider: ProviderMapping {
                    provider_id: "local-disk".to_string(),
                    item_id: "album/song.mp3".to_string(),
                },
                observed_metadata: vec![fixture_observation("Song", 1)],
                title: "Song".to_string(),
                artist_id: "artist_1".to_string(),
                artist_name: "Artist".to_string(),
                album_id: "album_1".to_string(),
                album_title: "Album".to_string(),
                year: Some(2026),
                track_number: Some(1),
                disc_number: None,
                extension: "mp3".to_string(),
                file_size_bytes: Some(1234),
                duration_seconds: Some(212.0),
                modified_at_unix_seconds: Some(1_800_000_000),
                content_hash: Some("abc123".to_string()),
                relative_path: "album/song.mp3".to_string(),
                stream_url: "/api/tracks/track_1/stream".to_string(),
                added_at_unix_seconds: None,
                path: PathBuf::from("/music/album/song.mp3"),
            }],
            scan_errors: vec![ScanIssue {
                path: "/music/bad".to_string(),
                message: "permission denied".to_string(),
            }],
        }
    }

    fn fixture_observation(title: &str, track_number: u16) -> TrackMetadataObservation {
        TrackMetadataObservation {
            source: "folder_path".to_string(),
            confidence: 0.55,
            observed_at_unix_seconds: 1_800_000_000,
            approval_state: MetadataApprovalState::Observed,
            field_observations: Vec::new(),
            title: Some(title.to_string()),
            artist_name: Some("Artist".to_string()),
            album_artist_name: Some("Album Artist".to_string()),
            album_title: Some("Album".to_string()),
            recording_date: Some("2026-02-03".to_string()),
            year: Some(2026),
            track_number: Some(track_number),
            track_total: Some(9),
            disc_number: Some(1),
            disc_total: Some(2),
            genres: vec!["Dub".to_string(), "Electronic".to_string()],
            composers: vec!["Composer".to_string()],
            lyrics: Some("Lyrics".to_string()),
            musicbrainz_recording_id: Some("recording-id".to_string()),
            musicbrainz_track_id: Some("track-id".to_string()),
            musicbrainz_release_id: Some("release-id".to_string()),
            musicbrainz_release_group_id: Some("release-group-id".to_string()),
            musicbrainz_artist_id: Some("artist-id".to_string()),
            musicbrainz_release_artist_id: Some("release-artist-id".to_string()),
            isrc: Some("USRC17607839".to_string()),
            embedded_artwork_count: 1,
        }
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("musicata-storage-{name}-{unique}.db"))
    }

    #[tokio::test]
    async fn saves_and_loads_a_player_queue() {
        use musicata_core::{PlaybackStatus, QueueItem, RepeatMode};

        let db_path = temp_db_path("player-queue");
        let database = Database::connect(&db_path).await.expect("connect");

        // No queue persisted yet.
        assert!(
            database
                .load_player_queue("p1")
                .await
                .expect("load")
                .is_none()
        );

        let items = vec![
            QueueItem {
                track_id: Some("track_1".to_string()),
                title: "One".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                stream_url: "/api/tracks/track_1/stream".to_string(),
                artwork_url: Some("/api/albums/a/cover".to_string()),
                ..Default::default()
            },
            QueueItem {
                track_id: None,
                title: "Stream".to_string(),
                artist: String::new(),
                album: String::new(),
                stream_url: "http://radio.example/stream".to_string(),
                artwork_url: None,
                ..Default::default()
            },
        ];
        let playback = super::PlayerPlayback {
            status: PlaybackStatus::Playing,
            position: Some(1),
            elapsed_seconds: Some(42.5),
            volume: Some(70),
            repeat: RepeatMode::All,
            shuffle: true,
        };
        database
            .save_player_queue("p1", &playback, &items)
            .await
            .expect("save queue");

        let loaded = database
            .load_player_queue("p1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.playback, playback);
        assert_eq!(loaded.items, items);

        // A playback-only save updates the row but leaves the items intact.
        let moved = super::PlayerPlayback {
            status: PlaybackStatus::Paused,
            position: Some(0),
            elapsed_seconds: Some(0.0),
            ..playback.clone()
        };
        database
            .save_player_playback("p1", &moved)
            .await
            .expect("save playback");
        let loaded = database
            .load_player_queue("p1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.playback, moved);
        assert_eq!(
            loaded.items, items,
            "items unchanged by a playback-only save"
        );

        // Shrinking the queue must not leave stale rows behind.
        database
            .save_player_queue("p1", &moved, &items[..1])
            .await
            .expect("save shorter queue");
        let loaded = database
            .load_player_queue("p1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.items.len(), 1);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn saves_and_loads_a_zone_queue() {
        use musicata_core::{PlaybackStatus, QueueItem, RepeatMode};

        let db_path = temp_db_path("zone-queue");
        let database = Database::connect(&db_path).await.expect("connect");

        // No queue persisted yet.
        assert!(
            database
                .load_zone_queue("z1")
                .await
                .expect("load")
                .is_none()
        );

        let items = vec![
            QueueItem {
                track_id: Some("track_1".to_string()),
                title: "One".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                stream_url: "/api/tracks/track_1/stream".to_string(),
                artwork_url: Some("/api/albums/a/cover".to_string()),
                ..Default::default()
            },
            QueueItem {
                track_id: Some("track_2".to_string()),
                title: "Two".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                stream_url: "/api/tracks/track_2/stream".to_string(),
                artwork_url: None,
                ..Default::default()
            },
        ];
        let playback = super::PlayerPlayback {
            status: PlaybackStatus::Playing,
            position: Some(1),
            elapsed_seconds: Some(12.0),
            volume: Some(55),
            repeat: RepeatMode::All,
            shuffle: true,
        };
        database
            .save_zone_queue("z1", &playback, &items)
            .await
            .expect("save zone queue");

        let loaded = database
            .load_zone_queue("z1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.playback, playback);
        assert_eq!(loaded.items, items);

        // A playback-only save updates the row but leaves the items intact.
        let moved = super::PlayerPlayback {
            status: PlaybackStatus::Paused,
            position: Some(0),
            elapsed_seconds: Some(0.0),
            ..playback.clone()
        };
        database
            .save_zone_playback("z1", &moved)
            .await
            .expect("save playback");
        let loaded = database
            .load_zone_queue("z1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.playback, moved);
        assert_eq!(
            loaded.items, items,
            "items unchanged by a playback-only save"
        );

        // Shrinking the queue must not leave stale rows behind.
        database
            .save_zone_queue("z1", &moved, &items[..1])
            .await
            .expect("save shorter queue");
        let loaded = database
            .load_zone_queue("z1")
            .await
            .expect("load")
            .expect("queue present");
        assert_eq!(loaded.items.len(), 1);

        let _ = std::fs::remove_file(db_path);
    }

    // A long-held write transaction (like the scan's full-library `save_library`
    // rewrite) must not block concurrent reads. Under the old rollback journal this
    // SELECT would wait on the writer and hit the busy-timeout; WAL lets it through.
    #[tokio::test]
    async fn a_held_write_does_not_block_reads() {
        let db_path = temp_db_path("wal-concurrency");
        let database = Database::connect(&db_path).await.expect("connect");

        // Confirm WAL is actually engaged (the whole point of the fix).
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&database.pool)
            .await
            .expect("journal_mode");
        assert_eq!(mode, "wal", "database must run in WAL mode");

        // Hold an exclusive write transaction open on its own connection.
        let mut writer = database.pool.acquire().await.expect("writer conn");
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .expect("begin write");
        sqlx::query("INSERT INTO favorites (item_type, item_id, created_at_unix_seconds) VALUES ('track','t1',0)")
            .execute(&mut *writer)
            .await
            .expect("write row");

        // A concurrent read on another pool connection must return promptly — well
        // under the 15s busy-timeout — rather than blocking on the open writer.
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tracks").fetch_one(&database.pool),
        )
        .await;
        assert!(
            matches!(read, Ok(Ok(_))),
            "a read must not block on a held write transaction (got {read:?})"
        );

        sqlx::query("ROLLBACK")
            .execute(&mut *writer)
            .await
            .expect("rollback");
        drop(writer);
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn acquired_artwork_lifecycle() {
        let db_path = temp_db_path("acquired-artwork");
        let database = Database::connect(&db_path).await.expect("connect");

        // A cover-less album (no folder/embedded artwork).
        let mut library = fixture_library();
        library.albums[0].artwork_url = None;
        library.albums[0].artwork_path = None;
        database.save_library(&mut library).await.expect("save");

        // It shows up as needing a fetch.
        let missing = database.albums_missing_artwork(0).await.expect("missing");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].album_id, "album_1");
        assert_eq!(missing[0].title, "Album");

        // Record a fetched cover.
        database
            .upsert_acquired_artwork(
                "album_1",
                "itunes",
                Some("https://example/cover/600x600.jpg"),
                Some("abc123key"),
                Some("jpg"),
                Some(600),
                "acquired",
                1_800_000_000,
            )
            .await
            .expect("upsert");

        let acquired = database
            .acquired_artwork("album_1")
            .await
            .expect("load")
            .expect("present");
        assert_eq!(acquired.status, "acquired");
        assert_eq!(acquired.cache_key.as_deref(), Some("abc123key"));
        assert_eq!(acquired.width, Some(600));

        // Now it's no longer "missing" (it's acquired, not awaiting a fetch).
        assert!(
            database
                .albums_missing_artwork(2_000_000_000)
                .await
                .expect("missing")
                .is_empty()
        );

        // Re-apply restores the album's artwork_url (a rescan would have wiped it).
        let restored = database.reapply_acquired_artwork().await.expect("reapply");
        assert_eq!(restored, 1);
        let album = database
            .album("album_1")
            .await
            .expect("album")
            .expect("some");
        assert_eq!(
            album.artwork_url.as_deref(),
            Some("/api/albums/album_1/artwork?asset=abc123key")
        );

        // A not_found marker suppresses re-querying until the retry window passes.
        database
            .upsert_acquired_artwork(
                "album_1",
                "none",
                None,
                None,
                None,
                None,
                "not_found",
                1_000,
            )
            .await
            .expect("upsert not_found");
        // Within the retry window (retry_before older than the marker): excluded.
        assert!(
            database
                .albums_missing_artwork(500)
                .await
                .expect("missing")
                .is_empty()
        );
        // Past the retry window (retry_before newer than the marker): eligible again.
        // (artwork_url was set above; clear it to model a coverless album.)
        database
            .set_album_artwork("album_1", None, None)
            .await
            .expect("clear url");
        assert_eq!(
            database
                .albums_missing_artwork(2_000)
                .await
                .expect("missing")
                .len(),
            1
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn acquired_artist_artwork_lifecycle() {
        let db_path = temp_db_path("acquired-artist-artwork");
        let database = Database::connect(&db_path).await.expect("connect");

        // fixture_library's artist starts with no image.
        let mut library = fixture_library();
        database.save_library(&mut library).await.expect("save");
        let artist_id = library.artists[0].id.clone();

        // It shows up as needing a fetch (ordered by track count, so highest-track first).
        let missing = database.artists_missing_artwork(0).await.expect("missing");
        assert!(missing.iter().any(|a| a.artist_id == artist_id));

        // Record a fetched image.
        database
            .upsert_acquired_artist_artwork(
                &artist_id,
                "deezer",
                Some("https://example/artist/1000.jpg"),
                Some("artkey1"),
                Some("jpg"),
                Some(1000),
                "acquired",
                1_800_000_000,
            )
            .await
            .expect("upsert");

        let acquired = database
            .acquired_artist_artwork(&artist_id)
            .await
            .expect("load")
            .expect("present");
        assert_eq!(acquired.status, "acquired");
        assert_eq!(acquired.cache_key.as_deref(), Some("artkey1"));

        // No longer "missing".
        assert!(
            !database
                .artists_missing_artwork(2_000_000_000)
                .await
                .expect("missing")
                .iter()
                .any(|a| a.artist_id == artist_id)
        );

        // Re-apply restores artwork_url (a rescan wipes the artists table).
        let restored = database
            .reapply_acquired_artist_artwork()
            .await
            .expect("reapply");
        assert_eq!(restored, 1);
        let artist = database
            .artist(&artist_id)
            .await
            .expect("artist")
            .expect("some");
        assert_eq!(
            artist.artwork_url,
            Some(format!("/api/artists/{artist_id}/artwork?asset=artkey1"))
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn remap_identity_moves_favorites_and_artwork() {
        let db_path = temp_db_path("remap-identity");
        let database = Database::connect(&db_path).await.expect("connect");

        // Seed two artist rows with stale ids whose names normalize to the SAME new id
        // ("Beyoncé" and "Beyonce") — modelling a pre-normalization DB.
        for (old_id, name) in [("artist_old1", "Beyoncé"), ("artist_old2", "Beyonce")] {
            sqlx::query(
                "INSERT INTO artists (id, name, album_count, track_count) VALUES (?1, ?2, 1, 1)",
            )
            .bind(old_id)
            .bind(name)
            .execute(&database.pool)
            .await
            .expect("insert artist");
        }
        let new_id = musicata_core::artist_identity("Beyoncé", None);
        assert_ne!(new_id, "artist_old1");

        // Star both stale artists; give one an acquired image.
        database.set_favorite("artist", "artist_old1", 1).await.unwrap();
        database.set_favorite("artist", "artist_old2", 2).await.unwrap();
        database
            .upsert_acquired_artist_artwork(
                "artist_old1", "deezer", None, Some("k"), Some("jpg"), Some(1000), "acquired", 9,
            )
            .await
            .unwrap();

        let moved = database.remap_identity_references().await.expect("remap");
        assert!(moved >= 1);

        // Both stars collapsed onto the single new id (no duplicate, no orphan).
        let fav_ids = database.favorite_ids("artist").await.unwrap();
        assert_eq!(fav_ids, vec![new_id.clone()]);
        // The acquired image moved to the new id.
        assert!(
            database
                .acquired_artist_artwork(&new_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            database
                .acquired_artist_artwork("artist_old1")
                .await
                .unwrap()
                .is_none()
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn artist_aliases_flatten_and_resolve() {
        let db_path = temp_db_path("artist-aliases");
        let database = Database::connect(&db_path).await.expect("connect");

        // b -> a, then chain c -> b should flatten to c -> a.
        database.add_artist_alias("b", "a", "Artist A", 1).await.unwrap();
        database.add_artist_alias("c", "b", "Artist B", 2).await.unwrap();
        let map = database.artist_alias_map().await.unwrap();
        assert_eq!(map.get("b").map(String::as_str), Some("Artist A"));
        // c flattened to the ultimate canonical name "Artist A" (not "Artist B").
        assert_eq!(map.get("c").map(String::as_str), Some("Artist A"));

        // Self-alias is a no-op; a cycle is rejected.
        database.add_artist_alias("a", "a", "Artist A", 3).await.unwrap();
        assert!(database.artist_alias_map().await.unwrap().get("a").is_none());
        assert!(database.add_artist_alias("a", "c", "x", 4).await.is_err());

        // Unmerge removes one member.
        database.remove_artist_alias("c").await.unwrap();
        assert!(database.artist_alias_map().await.unwrap().get("c").is_none());
        assert!(database.artist_alias_map().await.unwrap().get("b").is_some());

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn settings_round_trip() {
        let db_path = temp_db_path("settings");
        let database = Database::connect(&db_path).await.expect("connect");

        assert_eq!(
            database.get_setting("artwork_fetch").await.expect("get"),
            None
        );
        database
            .set_setting("artwork_fetch", "false")
            .await
            .expect("set");
        assert_eq!(
            database.get_setting("artwork_fetch").await.expect("get"),
            Some("false".to_string())
        );
        // Upsert overwrites.
        database
            .set_setting("artwork_fetch", "true")
            .await
            .expect("set");
        assert_eq!(
            database.get_setting("artwork_fetch").await.expect("get"),
            Some("true".to_string())
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn track_fingerprint_lifecycle() {
        let db_path = temp_db_path("track-fingerprint");
        let database = Database::connect(&db_path).await.expect("connect");

        // An untagged track: no MusicBrainz ids in its observation.
        let mut library = fixture_library();
        for obs in &mut library.tracks[0].observed_metadata {
            obs.musicbrainz_recording_id = None;
            obs.musicbrainz_track_id = None;
            obs.musicbrainz_release_id = None;
            obs.musicbrainz_release_group_id = None;
        }
        database.save_library(&mut library).await.expect("save");

        // It needs fingerprinting; the target carries enough to read the file.
        let missing = database
            .tracks_missing_fingerprints(0, 10)
            .await
            .expect("missing");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].track_id, "track_1");
        assert_eq!(missing[0].extension, "mp3");
        assert_eq!(missing[0].provider_item_id, "album/song.mp3");

        // No MBIDs for the album yet.
        assert_eq!(
            database
                .album_musicbrainz_ids("album_1")
                .await
                .expect("ids"),
            (None, None)
        );

        // Resolve via fingerprint.
        database
            .upsert_track_fingerprint(
                "track_1",
                "resolved",
                Some("rec"),
                Some("rel"),
                Some("rg"),
                1_800_000_000,
            )
            .await
            .expect("upsert");

        // No longer missing, and the album now resolves its MBIDs from the fingerprint.
        assert!(
            database
                .tracks_missing_fingerprints(2_000_000_000, 10)
                .await
                .expect("missing")
                .is_empty()
        );
        assert_eq!(
            database
                .album_musicbrainz_ids("album_1")
                .await
                .expect("ids"),
            (Some("rel".to_string()), Some("rg".to_string()))
        );

        // A not_found marker suppresses re-fingerprinting until the retry window passes.
        database
            .upsert_track_fingerprint("track_1", "not_found", None, None, None, 1_000)
            .await
            .expect("upsert");
        assert!(
            database
                .tracks_missing_fingerprints(500, 10)
                .await
                .expect("missing")
                .is_empty()
        );
        assert_eq!(
            database
                .tracks_missing_fingerprints(2_000, 10)
                .await
                .expect("missing")
                .len(),
            1
        );

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn track_loudness_lifecycle() {
        let db_path = temp_db_path("track-loudness");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = fixture_library();
        database.save_library(&mut library).await.expect("save");

        // A fresh track has no loudness yet — it's a candidate for analysis.
        let missing = database.tracks_missing_loudness(10).await.expect("missing");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].track_id, "track_1");
        assert!(database.track_loudness("track_1").await.expect("read").is_none());

        // Record a measurement; it reads back and the track is no longer queued.
        database
            .upsert_track_loudness("track_1", Some(-16.5), Some(-1.2), 1_800_000_000)
            .await
            .expect("upsert");
        assert_eq!(
            database.track_loudness("track_1").await.expect("read"),
            Some((-16.5, -1.2))
        );
        assert!(database.tracks_missing_loudness(10).await.expect("missing").is_empty());

        // A NULL marker (un-measurable track) still suppresses re-analysis but reads as None.
        database
            .upsert_track_loudness("track_1", None, None, 1_900_000_000)
            .await
            .expect("upsert null");
        assert!(database.track_loudness("track_1").await.expect("read").is_none());
        assert!(database.tracks_missing_loudness(10).await.expect("missing").is_empty());

        // A loaded queue gets its loudness re-attached from the table.
        database
            .upsert_track_loudness("track_1", Some(-12.0), Some(-0.5), 2_000_000_000)
            .await
            .expect("re-measure");
        let mut items = vec![musicata_core::QueueItem {
            track_id: Some("track_1".to_string()),
            ..Default::default()
        }];
        database.fill_queue_loudness(&mut items).await.expect("fill");
        assert_eq!(items[0].integrated_loudness_lufs, Some(-12.0));
        assert_eq!(items[0].true_peak_dbtp, Some(-0.5));

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn musicbrainz_reapply_regroups_and_survives_resave() {
        let db_path = temp_db_path("mb-enrich");
        let database = Database::connect(&db_path).await.expect("connect");

        // An untagged track: only a folder-derived observation (source "folder_path").
        let mut library = fixture_library();
        database.save_library(&mut library).await.expect("save");

        // Fingerprint resolved it; only resolved-fingerprint tracks are enrich targets.
        database
            .upsert_track_fingerprint("track_1", "resolved", Some("rec"), None, None, 1)
            .await
            .expect("fp");
        let target = database
            .tracks_missing_musicbrainz_metadata(0, 10)
            .await
            .expect("targets");
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].track_id, "track_1");
        assert_eq!(target[0].recording_mbid, "rec");

        // Enrichment fetched real MusicBrainz values.
        let meta = ResolvedMusicBrainzMetadata {
            title: Some("Real Title".to_string()),
            artist_name: Some("Real Artist".to_string()),
            album_title: Some("Real Album".to_string()),
            album_artist_name: Some("Real Artist".to_string()),
            track_number: Some(4),
            year: Some(1999),
            recording_mbid: Some("rec".to_string()),
            ..Default::default()
        };
        database
            .upsert_track_musicbrainz_metadata("track_1", "resolved", &meta, 1)
            .await
            .expect("mb");
        // No longer a target once enriched.
        assert!(
            database
                .tracks_missing_musicbrainz_metadata(0, 10)
                .await
                .expect("targets")
                .is_empty()
        );

        let applied = database
            .reapply_canonical_grouping()
            .await
            .expect("reapply");
        assert_eq!(applied, 1);

        // Canonical track fields updated AND the artist/album entities regrouped
        // consistently (the track points at the new entity rows; the old "Artist" is gone).
        let lib = database.load_library().await.expect("load").expect("lib");
        let track = &lib.tracks[0];
        assert_eq!(track.title, "Real Title");
        assert_eq!(track.artist_name, "Real Artist");
        assert_eq!(track.album_title, "Real Album");
        assert_eq!(track.track_number, Some(4));
        let artist = lib
            .artists
            .iter()
            .find(|a| a.name == "Real Artist")
            .expect("regrouped artist");
        assert_eq!(track.artist_id, artist.id);
        let album = lib
            .albums
            .iter()
            .find(|a| a.title == "Real Album")
            .expect("regrouped album");
        assert_eq!(track.album_id, album.id);
        assert!(lib.artists.iter().all(|a| a.name != "Artist"));

        // A later rescan rewrites the track back to folder-derived grouping...
        let mut rescanned = fixture_library();
        database.save_library(&mut rescanned).await.expect("resave");
        let lib = database.load_library().await.expect("load").expect("lib");
        assert_eq!(lib.tracks[0].title, "Song");

        // ...but reapply restores the enrichment (it must run after every save).
        database
            .reapply_canonical_grouping()
            .await
            .expect("reapply2");
        let lib = database.load_library().await.expect("load").expect("lib");
        assert_eq!(lib.tracks[0].title, "Real Title");

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn musicbrainz_reapply_never_clobbers_embedded_tags() {
        let db_path = temp_db_path("mb-enrich-embedded");
        let database = Database::connect(&db_path).await.expect("connect");

        // Track carries an embedded *title* tag (only title), plus its folder observation.
        let mut library = fixture_library();
        library.tracks[0].title = "Embedded Title".to_string();
        library
            .tracks[0]
            .observed_metadata
            .push(TrackMetadataObservation {
                source: "embedded_tag".to_string(),
                confidence: 0.95,
                observed_at_unix_seconds: 1_800_000_000,
                approval_state: MetadataApprovalState::Observed,
                field_observations: Vec::new(),
                title: Some("Embedded Title".to_string()),
                artist_name: None,
                album_artist_name: None,
                album_title: None,
                recording_date: None,
                year: None,
                track_number: None,
                track_total: None,
                disc_number: None,
                disc_total: None,
                genres: Vec::new(),
                composers: Vec::new(),
                lyrics: None,
                musicbrainz_recording_id: None,
                musicbrainz_track_id: None,
                musicbrainz_release_id: None,
                musicbrainz_release_group_id: None,
                musicbrainz_artist_id: None,
                musicbrainz_release_artist_id: None,
                isrc: None,
                embedded_artwork_count: 0,
            });
        database.save_library(&mut library).await.expect("save");

        database
            .upsert_track_fingerprint("track_1", "resolved", Some("rec"), None, None, 1)
            .await
            .expect("fp");
        let meta = ResolvedMusicBrainzMetadata {
            title: Some("MB Title".to_string()), // must NOT overwrite the embedded title
            artist_name: Some("MB Artist".to_string()), // folder-derived → may apply
            recording_mbid: Some("rec".to_string()),
            ..Default::default()
        };
        database
            .upsert_track_musicbrainz_metadata("track_1", "resolved", &meta, 1)
            .await
            .expect("mb");
        database
            .reapply_canonical_grouping()
            .await
            .expect("reapply");

        let lib = database.load_library().await.expect("load").expect("lib");
        let track = &lib.tracks[0];
        assert_eq!(track.title, "Embedded Title"); // embedded tag preserved
        assert_eq!(track.artist_name, "MB Artist"); // folder-derived field enriched

        let _ = std::fs::remove_file(db_path);
    }
}
