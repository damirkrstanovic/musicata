use anyhow::{Context, Result};
use musicata_core::{
    Album, Artist, Library, ProviderMapping, ScanIssue, Track, TrackMetadataObservation,
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

        Ok(())
    }

    pub async fn save_library(&self, library: &Library) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let result = async {
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
                    "INSERT INTO tracks (id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                .bind(&track.extension)
                .bind(track.file_size_bytes.map(u64_to_i64).transpose()?)
                .bind(track.modified_at_unix_seconds)
                .bind(&track.content_hash)
                .bind(&track.relative_path)
                .bind(&track.stream_url)
                .bind(path_to_string(&track.path))
                .execute(&mut *conn)
                .await?;

                for observation in &track.observed_metadata {
                    sqlx::query(
                        "INSERT INTO track_metadata_observations (track_id, source, title, artist_name, album_title, year, track_number) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )
                    .bind(&track.id)
                    .bind(&observation.source)
                    .bind(&observation.title)
                    .bind(&observation.artist_name)
                    .bind(&observation.album_title)
                    .bind(observation.year.map(i64::from))
                    .bind(observation.track_number.map(i64::from))
                    .execute(&mut *conn)
                    .await?;
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
            "SELECT id, provider_id, provider_item_id, title, artist_id, artist_name, album_id, album_title, year, track_number, extension, file_size_bytes, modified_at_unix_seconds, content_hash, relative_path, stream_url, path FROM tracks ORDER BY artist_name, year, album_title, track_number, title",
        )
        .fetch_all(&self.pool)
        .await?;
        let observation_rows = sqlx::query(
            "SELECT track_id, source, title, artist_name, album_title, year, track_number FROM track_metadata_observations ORDER BY track_id, id",
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

        let mut observations_by_track: BTreeMap<String, Vec<TrackMetadataObservation>> =
            BTreeMap::new();
        for row in observation_rows {
            let track_id: String = row.try_get("track_id")?;
            observations_by_track
                .entry(track_id)
                .or_default()
                .push(TrackMetadataObservation {
                    source: row.try_get("source")?,
                    title: row.try_get("title")?,
                    artist_name: row.try_get("artist_name")?,
                    album_title: row.try_get("album_title")?,
                    year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                    track_number: optional_i64_to_u16(
                        row.try_get("track_number")?,
                        "track_number",
                    )?,
                });
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
                extension: row.try_get("extension")?,
                file_size_bytes: optional_i64_to_u64(
                    row.try_get("file_size_bytes")?,
                    "file_size_bytes",
                )?,
                modified_at_unix_seconds: row.try_get("modified_at_unix_seconds")?,
                content_hash: row.try_get("content_hash")?,
                relative_path: row.try_get("relative_path")?,
                stream_url: row.try_get("stream_url")?,
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
            "SELECT track_id, source, title, artist_name, album_title, year, track_number FROM track_metadata_observations ORDER BY track_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut stored_observations: BTreeMap<String, String> = BTreeMap::new();
        for row in observation_rows {
            let track_id: String = row.try_get("track_id")?;
            let observation = TrackMetadataObservation {
                source: row.try_get("source")?,
                title: row.try_get("title")?,
                artist_name: row.try_get("artist_name")?,
                album_title: row.try_get("album_title")?,
                year: optional_i64_to_u16(row.try_get("year")?, "year")?,
                track_number: optional_i64_to_u16(row.try_get("track_number")?, "track_number")?,
            };
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
            observation.album_title.as_deref().unwrap_or_default(),
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
        extension TEXT NOT NULL,
        file_size_bytes INTEGER,
        modified_at_unix_seconds INTEGER,
        content_hash TEXT,
        relative_path TEXT NOT NULL,
        stream_url TEXT NOT NULL,
        path TEXT NOT NULL
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
        title TEXT,
        artist_name TEXT,
        album_title TEXT,
        year INTEGER,
        track_number INTEGER,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id)",
    "CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id)",
    "CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id)",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_observations_track_id ON track_metadata_observations(track_id)",
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
        title TEXT,
        artist_name TEXT,
        album_title TEXT,
        year INTEGER,
        track_number INTEGER,
        FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
    )",
    "CREATE INDEX IF NOT EXISTS idx_track_metadata_observations_track_id ON track_metadata_observations(track_id)",
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

fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("file size does not fit in SQLite INTEGER")
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
        Album, Artist, Library, ProviderMapping, ScanIssue, Track, TrackMetadataObservation,
    };
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn saves_and_loads_library() {
        let db_path = temp_db_path("roundtrip");
        let database = Database::connect(&db_path).await.expect("connect database");
        let library = fixture_library();

        database.save_library(&library).await.expect("save library");
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
        assert_eq!(
            loaded.tracks[0].observed_metadata[0].title,
            Some("Song".to_string())
        );
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
    async fn detects_added_removed_and_modified_tracks() {
        let db_path = temp_db_path("changes");
        let database = Database::connect(&db_path).await.expect("connect database");
        let library = fixture_library();
        database.save_library(&library).await.expect("save library");

        let mut metadata_changed = library.clone();
        metadata_changed.tracks[0].observed_metadata[0].title = Some("Retagged".to_string());
        let changes = database
            .detect_library_changes(&metadata_changed)
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
            extension: "mp3".to_string(),
            file_size_bytes: Some(10),
            modified_at_unix_seconds: Some(1_800_000_100),
            content_hash: Some("new789".to_string()),
            relative_path: "album/new-song.mp3".to_string(),
            stream_url: "/api/tracks/track_2/stream".to_string(),
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
                extension: "mp3".to_string(),
                file_size_bytes: Some(1234),
                modified_at_unix_seconds: Some(1_800_000_000),
                content_hash: Some("abc123".to_string()),
                relative_path: "album/song.mp3".to_string(),
                stream_url: "/api/tracks/track_1/stream".to_string(),
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
            title: Some(title.to_string()),
            artist_name: Some("Artist".to_string()),
            album_title: Some("Album".to_string()),
            year: Some(2026),
            track_number: Some(track_number),
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
