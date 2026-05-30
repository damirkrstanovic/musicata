use anyhow::{Context, Result};
use musicata_core::{
    Album, Artist, Library, MetadataApprovalState, MetadataFieldValue, ProviderMapping, ScanIssue,
    SearchResults, Track, TrackMetadataFieldObservation, TrackMetadataObservation,
};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
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

impl Database {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
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
            .foreign_keys(true);
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

        Ok(())
    }

    pub async fn save_library(&self, library: &mut Library) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let result = async {
            // Preserve each track's original "added at" timestamp across the full
            // delete/re-insert by keying on the stable provider item id.
            let mut existing_added_at: BTreeMap<String, Option<i64>> = BTreeMap::new();
            let existing_rows =
                sqlx::query("SELECT provider_item_id, added_at_unix_seconds FROM tracks")
                    .fetch_all(&mut *conn)
                    .await?;
            for row in existing_rows {
                existing_added_at.insert(
                    row.try_get("provider_item_id")?,
                    row.try_get("added_at_unix_seconds")?,
                );
            }
            let now = now_unix_seconds();
            for track in &mut library.tracks {
                let added_at = track
                    .added_at_unix_seconds
                    .or_else(|| {
                        existing_added_at
                            .get(&track.provider.item_id)
                            .copied()
                            .flatten()
                    })
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
                    "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, disc_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path, added_at_unix_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
                Err(error.into())
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
            "SELECT id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, disc_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path, added_at_unix_seconds FROM tracks ORDER BY artist_name, year, album_title, disc_number, track_number, title",
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
    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let Some(match_query) = fts_match_query(query) else {
            return Ok(SearchResults::default());
        };
        let limit = limit as i64;

        let artist_rows = sqlx::query(
            "SELECT a.id, a.name, a.album_count, a.track_count
             FROM artists_fts f JOIN artists a ON a.rowid = f.rowid
             WHERE artists_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )
        .bind(&match_query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut artists = Vec::with_capacity(artist_rows.len());
        for row in artist_rows {
            artists.push(Artist {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                album_count: i64_to_usize(row.try_get("album_count")?, "album_count")?,
                track_count: i64_to_usize(row.try_get("track_count")?, "track_count")?,
            });
        }

        let album_rows = sqlx::query(
            "SELECT a.id, a.title, a.artist_id, a.artist_name, a.year, a.track_count, a.artwork_url, a.artwork_path
             FROM albums_fts f JOIN albums a ON a.rowid = f.rowid
             WHERE albums_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )
        .bind(&match_query)
        .bind(limit)
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
                    t.stream_url, t.added_at_unix_seconds, t.path
             FROM tracks_fts f JOIN tracks t ON t.rowid = f.rowid
             WHERE tracks_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )
        .bind(&match_query)
        .bind(limit)
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
    use super::Database;
    use musicata_core::{
        Album, Artist, Library, MetadataApprovalState, MetadataFieldValue, ProviderMapping,
        ScanIssue, Track, TrackMetadataObservation,
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

        let by_track = database.search("song", 50).await.expect("search");
        assert!(by_track.tracks.iter().any(|track| track.title == "Song"));

        let by_album = database.search("album", 50).await.expect("search");
        assert!(by_album.albums.iter().any(|album| album.title == "Album"));

        let by_artist = database.search("artist", 50).await.expect("search");
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

        let results = database.search("   ", 50).await.expect("search");
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
            let results = database.search(query, 50).await.expect("search");
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
        let results = database.search("alb", 50).await.expect("search");
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

        let results = database.search("zephyr", 50).await.expect("search");
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
}
