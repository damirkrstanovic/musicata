//! Core domain and provider interfaces for Musicata.
//!
//! The initial implementation ships a local-disk provider, but the domain model
//! deliberately describes music independently from the source that provided it.

use lofty::{
    config::ParseOptions, file::AudioFile, file::TaggedFileExt, prelude::Accessor, probe::Probe,
    tag::ItemKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display},
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const LYRIC_SIDECARS: &[(&str, &str, f32)] =
    &[("lrc", "sidecar_lrc", 0.9), ("txt", "sidecar_txt", 0.8)];
// Full-file hashing is too expensive for first imports of mounted multi-GB libraries.
// Keep synchronous hashes for tiny files and move deep fingerprinting to a later worker.
const INLINE_CONTENT_HASH_LIMIT_BYTES: u64 = 1024 * 1024;

pub trait MusicProvider {
    fn provider_id(&self) -> &str;
    fn scan(&self) -> Result<Library, ScanError>;
}

#[derive(Clone, Debug)]
pub struct LocalDiskProvider {
    root: PathBuf,
}

impl LocalDiskProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl MusicProvider for LocalDiskProvider {
    fn provider_id(&self) -> &str {
        "local-disk"
    }

    fn scan(&self) -> Result<Library, ScanError> {
        scan_local_library(&self.root)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Library {
    pub provider_id: String,
    pub source_root: String,
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
    pub scan_errors: Vec<ScanIssue>,
}

impl Library {
    pub fn summary(&self) -> LibrarySummary {
        LibrarySummary {
            provider_id: self.provider_id.clone(),
            source_root: self.source_root.clone(),
            artist_count: self.artists.len(),
            album_count: self.albums.len(),
            track_count: self.tracks.len(),
        }
    }

    pub fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|track| track.id == id)
    }

    pub fn album(&self, id: &str) -> Option<&Album> {
        self.albums.iter().find(|album| album.id == id)
    }

    pub fn artist(&self, id: &str) -> Option<&Artist> {
        self.artists.iter().find(|artist| artist.id == id)
    }

    pub fn browse_index(&self) -> BrowseIndex {
        let mut genres = BTreeMap::new();
        let mut years = BTreeMap::new();
        let mut composers = BTreeMap::new();
        let mut folders = BTreeMap::new();

        for track in &self.tracks {
            for genre in track_genres(track) {
                *genres.entry(genre).or_insert(0) += 1;
            }
            if let Some(year) = track.year {
                *years.entry(year).or_insert(0) += 1;
            }
            for composer in track_composers(track) {
                *composers.entry(composer).or_insert(0) += 1;
            }
            if let Some(folder) = track_folder(track) {
                *folders.entry(folder.to_string()).or_insert(0) += 1;
            }
        }

        BrowseIndex {
            genres: genres
                .into_iter()
                .map(|(value, track_count)| BrowseTextFacet { value, track_count })
                .collect(),
            years: years
                .into_iter()
                .rev()
                .map(|(value, track_count)| BrowseYearFacet { value, track_count })
                .collect(),
            composers: composers
                .into_iter()
                .map(|(value, track_count)| BrowseTextFacet { value, track_count })
                .collect(),
            folders: folders
                .into_iter()
                .map(|(value, track_count)| BrowseTextFacet { value, track_count })
                .collect(),
        }
    }

    pub fn browse_tracks(&self, filter: &BrowseFilter) -> Vec<Track> {
        self.tracks
            .iter()
            .filter(|track| {
                filter.year.is_none_or(|year| track.year == Some(year))
                    && text_filter_matches(filter.genre.as_deref(), track_genres(track))
                    && text_filter_matches(filter.composer.as_deref(), track_composers(track))
                    && folder_filter_matches(filter.folder.as_deref(), track)
            })
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LibrarySummary {
    pub provider_id: String,
    pub source_root: String,
    pub artist_count: usize,
    pub album_count: usize,
    pub track_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub album_count: usize,
    pub track_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub year: Option<u16>,
    pub track_count: usize,
    pub artwork_url: Option<String>,
    #[serde(skip)]
    pub artwork_path: Option<PathBuf>,
}

/// A user-created playlist. Its tracks are stored separately (ordered) and returned
/// with the detail view.
#[derive(Clone, Debug, Serialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: Option<String>,
    pub song_count: usize,
    pub created_at_unix_seconds: i64,
    pub updated_at_unix_seconds: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Track {
    pub id: String,
    pub provider: ProviderMapping,
    pub observed_metadata: Vec<TrackMetadataObservation>,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub album_id: String,
    pub album_title: String,
    pub year: Option<u16>,
    pub track_number: Option<u16>,
    pub disc_number: Option<u16>,
    pub extension: String,
    pub file_size_bytes: Option<u64>,
    /// Track length in seconds, read from the audio stream at scan time. `None` when
    /// the file's properties couldn't be decoded.
    pub duration_seconds: Option<f64>,
    pub modified_at_unix_seconds: Option<i64>,
    pub content_hash: Option<String>,
    pub relative_path: String,
    pub stream_url: String,
    /// When this track first entered the database, in Unix seconds. Assigned by
    /// the storage layer (the scanner has no view of database history), so it is
    /// `None` on freshly scanned tracks until they are saved.
    pub added_at_unix_seconds: Option<i64>,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanIssue {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderMapping {
    pub provider_id: String,
    pub item_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrackMetadataObservation {
    pub source: String,
    pub confidence: f32,
    pub observed_at_unix_seconds: i64,
    pub approval_state: MetadataApprovalState,
    pub field_observations: Vec<TrackMetadataFieldObservation>,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_artist_name: Option<String>,
    pub album_title: Option<String>,
    pub recording_date: Option<String>,
    pub year: Option<u16>,
    pub track_number: Option<u16>,
    pub track_total: Option<u16>,
    pub disc_number: Option<u16>,
    pub disc_total: Option<u16>,
    pub genres: Vec<String>,
    pub composers: Vec<String>,
    pub lyrics: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_track_id: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
    pub musicbrainz_release_artist_id: Option<String>,
    pub isrc: Option<String>,
    pub embedded_artwork_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrackMetadataFieldObservation {
    pub source: String,
    pub field_name: String,
    pub value: MetadataFieldValue,
    pub confidence: f32,
    pub observed_at_unix_seconds: i64,
    pub approval_state: MetadataApprovalState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum MetadataFieldValue {
    Text(String),
    Number(u16),
    TextList(Vec<String>),
    Count(usize),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataApprovalState {
    Observed,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchResults {
    pub query: String,
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BrowseFilter {
    pub genre: Option<String>,
    pub year: Option<u16>,
    pub composer: Option<String>,
    pub folder: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BrowseIndex {
    pub genres: Vec<BrowseTextFacet>,
    pub years: Vec<BrowseYearFacet>,
    pub composers: Vec<BrowseTextFacet>,
    pub folders: Vec<BrowseTextFacet>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BrowseTextFacet {
    pub value: String,
    pub track_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BrowseYearFacet {
    pub value: u16,
    pub track_count: usize,
}

// ---------------------------------------------------------------------------
// Player domain
//
// Provider-neutral types describing controllable playback endpoints (a player),
// their live state, and the commands a controller can issue. The local-disk MPD
// backend is the first concrete provider; these types do not depend on it.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PlayerCapabilities {
    pub seek: bool,
    pub volume: bool,
    pub repeat: bool,
    pub shuffle: bool,
    pub queue: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Player {
    pub id: String,
    pub name: String,
    /// Backend kind, e.g. "mpd".
    pub kind: String,
    /// Backend address the server connects to, e.g. `host:port` for MPD.
    pub address: String,
    /// Zone this player belongs to, if any.
    pub zone_id: Option<String>,
    pub online: bool,
    pub capabilities: PlayerCapabilities,
}

/// A named group of players. Commands sent to a zone apply to its players; there
/// is no audio synchronization between them.
#[derive(Clone, Debug, Serialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

/// What a controller knows about a track queued on a player. `stream_url` is what
/// the player actually fetches; `track_id` links back to the library when known.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct QueueItem {
    pub track_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub stream_url: String,
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub now_playing: Option<QueueItem>,
    pub elapsed_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub volume: Option<u8>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub queue: Vec<QueueItem>,
    pub queue_position: Option<usize>,
}

/// A command issued to a player. `PlayTracks`/`Enqueue` reference library track
/// ids; the server resolves them to stream URLs and metadata before dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "command")]
pub enum PlayerCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Seek { position_seconds: f64 },
    SetVolume { volume: u8 },
    SetRepeat { mode: RepeatMode },
    SetShuffle { enabled: bool },
    Clear,
    PlayTracks { track_ids: Vec<String> },
    Enqueue { track_ids: Vec<String> },
    PlayQueueIndex { index: usize },
    RemoveQueueItem { index: usize },
    MoveQueueItem { from: usize, to: usize },
}

#[derive(Debug)]
pub enum ScanError {
    NotFound(PathBuf),
    NotDirectory(PathBuf),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "library path does not exist: {}", path.display()),
            Self::NotDirectory(path) => {
                write!(f, "library path is not a directory: {}", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn scan_local_library(root: &Path) -> Result<Library, ScanError> {
    if !root.exists() {
        return Err(ScanError::NotFound(root.to_path_buf()));
    }

    if !root.is_dir() {
        return Err(ScanError::NotDirectory(root.to_path_buf()));
    }

    let root = root.canonicalize().map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let mut files = Vec::new();
    let mut scan_errors = Vec::new();
    collect_audio_files(&root, &mut files, &mut scan_errors, true)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let mut tracks = Vec::new();
    let mut album_builders: BTreeMap<String, AlbumBuilder> = BTreeMap::new();
    let mut artist_builders: BTreeMap<String, ArtistBuilder> = BTreeMap::new();
    let mut track_id_counts = BTreeMap::new();
    let observed_at_unix_seconds = current_unix_seconds();

    for file in files {
        let path = file.path;
        let folder_metadata = infer_track_metadata(&root, &path);
        let (embedded_metadata, duration_seconds) =
            read_embedded_metadata(&path, &mut scan_errors, observed_at_unix_seconds);
        let sidecar_lyrics = read_sidecar_lyrics(&path, &mut scan_errors, observed_at_unix_seconds);
        let metadata = canonical_metadata(embedded_metadata.as_ref(), &folder_metadata);
        let observed_metadata = metadata_observations(
            embedded_metadata,
            sidecar_lyrics,
            &folder_metadata,
            observed_at_unix_seconds,
        );
        let artist_id = stable_id("artist", &metadata.artist_name.to_ascii_lowercase());
        let album_artist_name = metadata
            .album_artist_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| metadata.artist_name.clone());
        let album_artist_id = stable_id("artist", &album_artist_name.to_ascii_lowercase());
        let album_key = format!(
            "{}::{}",
            album_artist_name.to_ascii_lowercase(),
            metadata.album_title.to_ascii_lowercase()
        );
        let album_id = stable_id("album", &album_key);
        let relative_path = relative_display_path(&root, &path);
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let track_identity = build_track_identity(
            &metadata,
            &extension,
            file.file_size_bytes,
            file.modified_at_unix_seconds,
            file.content_hash.as_deref(),
        );
        let track_id = unique_track_id(&track_identity, &mut track_id_counts);

        album_builders
            .entry(album_id.clone())
            .or_insert_with(|| AlbumBuilder {
                id: album_id.clone(),
                title: metadata.album_title.clone(),
                artist_id: album_artist_id.clone(),
                artist_name: album_artist_name.clone(),
                year: metadata.year,
                track_count: 0,
                artwork_path: find_album_artwork(path.parent().unwrap_or(&root)),
            })
            .track_count += 1;

        // Count the track against its own (track) artist.
        artist_builders
            .entry(artist_id.clone())
            .or_insert_with(|| ArtistBuilder {
                id: artist_id.clone(),
                name: metadata.artist_name.clone(),
                album_ids: Vec::new(),
                track_count: 0,
            })
            .track_count += 1;

        // Associate the album with its album-artist (the same entry for non-compilations).
        let owning_artist = artist_builders
            .entry(album_artist_id.clone())
            .or_insert_with(|| ArtistBuilder {
                id: album_artist_id.clone(),
                name: album_artist_name.clone(),
                album_ids: Vec::new(),
                track_count: 0,
            });
        if !owning_artist.album_ids.contains(&album_id) {
            owning_artist.album_ids.push(album_id.clone());
        }

        tracks.push(Track {
            id: track_id.clone(),
            provider: ProviderMapping {
                provider_id: "local-disk".to_string(),
                item_id: relative_path.clone(),
            },
            observed_metadata,
            title: metadata.title,
            artist_id,
            artist_name: metadata.artist_name,
            album_id,
            album_title: metadata.album_title,
            year: metadata.year,
            track_number: metadata.track_number,
            disc_number: metadata.disc_number,
            extension,
            file_size_bytes: file.file_size_bytes,
            duration_seconds,
            modified_at_unix_seconds: file.modified_at_unix_seconds,
            content_hash: file.content_hash,
            relative_path,
            stream_url: format!("/api/tracks/{}/stream", track_id),
            added_at_unix_seconds: None,
            path,
        });
    }

    let mut artists: Vec<_> = artist_builders
        .into_values()
        .map(|artist| Artist {
            id: artist.id,
            name: artist.name,
            album_count: artist.album_ids.len(),
            track_count: artist.track_count,
        })
        .collect();
    artists.sort_by(|left, right| left.name.cmp(&right.name));

    let mut albums: Vec<_> = album_builders
        .into_values()
        .map(|album| {
            let artwork_url = album
                .artwork_path
                .as_ref()
                .map(|path| album_artwork_url(&album.id, path));
            Album {
                id: album.id,
                title: album.title,
                artist_id: album.artist_id,
                artist_name: album.artist_name,
                year: album.year,
                track_count: album.track_count,
                artwork_url,
                artwork_path: album.artwork_path,
            }
        })
        .collect();
    albums.sort_by(|left, right| {
        left.artist_name
            .cmp(&right.artist_name)
            .then_with(|| left.year.cmp(&right.year))
            .then_with(|| left.title.cmp(&right.title))
    });

    tracks.sort_by(|left, right| {
        left.artist_name
            .cmp(&right.artist_name)
            .then_with(|| left.year.cmp(&right.year))
            .then_with(|| left.album_title.cmp(&right.album_title))
            .then_with(|| left.disc_number.cmp(&right.disc_number))
            .then_with(|| left.track_number.cmp(&right.track_number))
            .then_with(|| left.title.cmp(&right.title))
    });

    Ok(Library {
        provider_id: "local-disk".to_string(),
        source_root: root.display().to_string(),
        artists,
        albums,
        tracks,
        scan_errors,
    })
}

#[derive(Clone, Debug)]
struct DiscoveredAudioFile {
    path: PathBuf,
    file_size_bytes: Option<u64>,
    modified_at_unix_seconds: Option<i64>,
    content_hash: Option<String>,
}

#[derive(Clone, Debug)]
struct TrackMetadata {
    title: String,
    artist_name: String,
    album_artist_name: Option<String>,
    album_title: String,
    year: Option<u16>,
    track_number: Option<u16>,
    disc_number: Option<u16>,
}

impl TrackMetadataObservation {
    fn folder_path(metadata: &TrackMetadata, observed_at_unix_seconds: i64) -> Self {
        Self {
            source: "folder_path".to_string(),
            confidence: 0.55,
            observed_at_unix_seconds,
            approval_state: MetadataApprovalState::Observed,
            field_observations: Vec::new(),
            title: Some(metadata.title.clone()),
            artist_name: Some(metadata.artist_name.clone()),
            album_artist_name: None,
            album_title: Some(metadata.album_title.clone()),
            recording_date: metadata.year.map(|year| year.to_string()),
            year: metadata.year,
            track_number: metadata.track_number,
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
        }
        .with_default_field_observations()
    }

    fn embedded_tag(tag: &lofty::tag::Tag, observed_at_unix_seconds: i64) -> Option<Self> {
        let album_artist_name = clean_tag_value(tag, ItemKey::AlbumArtist)
            .or_else(|| first_clean_tag_value(tag, ItemKey::AlbumArtists));

        let observation = Self {
            source: "embedded_tag".to_string(),
            confidence: 0.95,
            observed_at_unix_seconds,
            approval_state: MetadataApprovalState::Observed,
            field_observations: Vec::new(),
            title: clean_optional_tag_value(tag.title().as_deref()),
            artist_name: clean_optional_tag_value(tag.artist().as_deref()),
            album_artist_name,
            album_title: clean_optional_tag_value(tag.album().as_deref()),
            recording_date: tag.date().map(|date| date.to_string()),
            year: tag.date().map(|date| date.year),
            track_number: tag.track().and_then(u32_to_u16),
            track_total: tag.track_total().and_then(u32_to_u16),
            disc_number: tag.disk().and_then(u32_to_u16),
            disc_total: tag.disk_total().and_then(u32_to_u16),
            genres: clean_tag_values(tag.get_strings(ItemKey::Genre)),
            composers: clean_tag_values(tag.get_strings(ItemKey::Composer)),
            lyrics: clean_tag_lyrics_value(tag, ItemKey::Lyrics)
                .or_else(|| clean_tag_lyrics_value(tag, ItemKey::UnsyncLyrics)),
            musicbrainz_recording_id: clean_tag_value(tag, ItemKey::MusicBrainzRecordingId),
            musicbrainz_track_id: clean_tag_value(tag, ItemKey::MusicBrainzTrackId),
            musicbrainz_release_id: clean_tag_value(tag, ItemKey::MusicBrainzReleaseId),
            musicbrainz_release_group_id: clean_tag_value(tag, ItemKey::MusicBrainzReleaseGroupId),
            musicbrainz_artist_id: clean_tag_value(tag, ItemKey::MusicBrainzArtistId),
            musicbrainz_release_artist_id: clean_tag_value(
                tag,
                ItemKey::MusicBrainzReleaseArtistId,
            ),
            isrc: clean_tag_value(tag, ItemKey::Isrc),
            embedded_artwork_count: tag.pictures().len(),
        };

        observation
            .has_metadata()
            .then(|| observation.with_default_field_observations())
    }

    fn sidecar_lyrics(
        source: &str,
        confidence: f32,
        lyrics: String,
        observed_at_unix_seconds: i64,
    ) -> Self {
        Self {
            source: source.to_string(),
            confidence,
            observed_at_unix_seconds,
            approval_state: MetadataApprovalState::Observed,
            field_observations: Vec::new(),
            title: None,
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
            lyrics: Some(lyrics),
            musicbrainz_recording_id: None,
            musicbrainz_track_id: None,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            musicbrainz_artist_id: None,
            musicbrainz_release_artist_id: None,
            isrc: None,
            embedded_artwork_count: 0,
        }
        .with_default_field_observations()
    }

    pub fn effective_field_observations(&self) -> Vec<TrackMetadataFieldObservation> {
        if self.field_observations.is_empty() {
            self.default_field_observations()
        } else {
            self.field_observations.clone()
        }
    }

    fn has_metadata(&self) -> bool {
        self.title.is_some()
            || self.artist_name.is_some()
            || self.album_title.is_some()
            || self.album_artist_name.is_some()
            || self.recording_date.is_some()
            || self.year.is_some()
            || self.track_number.is_some()
            || self.track_total.is_some()
            || self.disc_number.is_some()
            || self.disc_total.is_some()
            || !self.genres.is_empty()
            || !self.composers.is_empty()
            || self.lyrics.is_some()
            || self.musicbrainz_recording_id.is_some()
            || self.musicbrainz_track_id.is_some()
            || self.musicbrainz_release_id.is_some()
            || self.musicbrainz_release_group_id.is_some()
            || self.musicbrainz_artist_id.is_some()
            || self.musicbrainz_release_artist_id.is_some()
            || self.isrc.is_some()
            || self.embedded_artwork_count > 0
    }

    fn with_default_field_observations(mut self) -> Self {
        self.field_observations = self.default_field_observations();
        self
    }

    fn default_field_observations(&self) -> Vec<TrackMetadataFieldObservation> {
        let mut fields = Vec::new();

        self.push_text_field(&mut fields, "title", self.title.as_deref());
        self.push_text_field(&mut fields, "artist_name", self.artist_name.as_deref());
        self.push_text_field(
            &mut fields,
            "album_artist_name",
            self.album_artist_name.as_deref(),
        );
        self.push_text_field(&mut fields, "album_title", self.album_title.as_deref());
        self.push_text_field(
            &mut fields,
            "recording_date",
            self.recording_date.as_deref(),
        );
        self.push_number_field(&mut fields, "year", self.year);
        self.push_number_field(&mut fields, "track_number", self.track_number);
        self.push_number_field(&mut fields, "track_total", self.track_total);
        self.push_number_field(&mut fields, "disc_number", self.disc_number);
        self.push_number_field(&mut fields, "disc_total", self.disc_total);
        self.push_text_list_field(&mut fields, "genres", &self.genres);
        self.push_text_list_field(&mut fields, "composers", &self.composers);
        self.push_text_field(&mut fields, "lyrics", self.lyrics.as_deref());
        self.push_text_field(
            &mut fields,
            "musicbrainz_recording_id",
            self.musicbrainz_recording_id.as_deref(),
        );
        self.push_text_field(
            &mut fields,
            "musicbrainz_track_id",
            self.musicbrainz_track_id.as_deref(),
        );
        self.push_text_field(
            &mut fields,
            "musicbrainz_release_id",
            self.musicbrainz_release_id.as_deref(),
        );
        self.push_text_field(
            &mut fields,
            "musicbrainz_release_group_id",
            self.musicbrainz_release_group_id.as_deref(),
        );
        self.push_text_field(
            &mut fields,
            "musicbrainz_artist_id",
            self.musicbrainz_artist_id.as_deref(),
        );
        self.push_text_field(
            &mut fields,
            "musicbrainz_release_artist_id",
            self.musicbrainz_release_artist_id.as_deref(),
        );
        self.push_text_field(&mut fields, "isrc", self.isrc.as_deref());

        if self.embedded_artwork_count > 0 {
            self.push_field(
                &mut fields,
                "embedded_artwork_count",
                MetadataFieldValue::Count(self.embedded_artwork_count),
            );
        }

        fields
    }

    fn push_text_field(
        &self,
        fields: &mut Vec<TrackMetadataFieldObservation>,
        field_name: &str,
        value: Option<&str>,
    ) {
        if let Some(value) = value {
            self.push_field(
                fields,
                field_name,
                MetadataFieldValue::Text(value.to_string()),
            );
        }
    }

    fn push_number_field(
        &self,
        fields: &mut Vec<TrackMetadataFieldObservation>,
        field_name: &str,
        value: Option<u16>,
    ) {
        if let Some(value) = value {
            self.push_field(fields, field_name, MetadataFieldValue::Number(value));
        }
    }

    fn push_text_list_field(
        &self,
        fields: &mut Vec<TrackMetadataFieldObservation>,
        field_name: &str,
        values: &[String],
    ) {
        if !values.is_empty() {
            self.push_field(
                fields,
                field_name,
                MetadataFieldValue::TextList(values.to_vec()),
            );
        }
    }

    fn push_field(
        &self,
        fields: &mut Vec<TrackMetadataFieldObservation>,
        field_name: &str,
        value: MetadataFieldValue,
    ) {
        fields.push(TrackMetadataFieldObservation {
            source: self.source.clone(),
            field_name: field_name.to_string(),
            value,
            confidence: self.confidence,
            observed_at_unix_seconds: self.observed_at_unix_seconds,
            approval_state: self.approval_state.clone(),
        });
    }
}

#[derive(Clone, Debug)]
struct AlbumBuilder {
    id: String,
    title: String,
    artist_id: String,
    artist_name: String,
    year: Option<u16>,
    track_count: usize,
    artwork_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct ArtistBuilder {
    id: String,
    name: String,
    album_ids: Vec<String>,
    track_count: usize,
}

fn collect_audio_files(
    root: &Path,
    files: &mut Vec<DiscoveredAudioFile>,
    scan_errors: &mut Vec<ScanIssue>,
    required: bool,
) -> Result<(), ScanError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if required => {
            return Err(ScanError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
        Err(source) => {
            scan_errors.push(ScanIssue {
                path: root.display().to_string(),
                message: source.to_string(),
            });
            return Ok(());
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                scan_errors.push(ScanIssue {
                    path: root.display().to_string(),
                    message: source.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(source) => {
                scan_errors.push(ScanIssue {
                    path: path.display().to_string(),
                    message: source.to_string(),
                });
                continue;
            }
        };

        if file_type.is_dir() {
            collect_audio_files(&path, files, scan_errors, false)?;
        } else if file_type.is_file() && has_extension(&path, AUDIO_EXTENSIONS) {
            let file_metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    scan_errors.push(ScanIssue {
                        path: path.display().to_string(),
                        message: source.to_string(),
                    });
                    files.push(DiscoveredAudioFile {
                        path,
                        file_size_bytes: None,
                        modified_at_unix_seconds: None,
                        content_hash: None,
                    });
                    continue;
                }
            };
            let content_hash = match hash_file_contents_if_small(&path, file_metadata.len()) {
                Ok(hash) => hash,
                Err(source) => {
                    scan_errors.push(ScanIssue {
                        path: path.display().to_string(),
                        message: source.to_string(),
                    });
                    None
                }
            };
            files.push(DiscoveredAudioFile {
                path,
                file_size_bytes: Some(file_metadata.len()),
                modified_at_unix_seconds: file_metadata
                    .modified()
                    .ok()
                    .and_then(system_time_to_unix_seconds),
                content_hash,
            });
        }
    }

    Ok(())
}

fn system_time_to_unix_seconds(time: std::time::SystemTime) -> Option<i64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn hash_file_contents_if_small(
    path: &Path,
    file_size_bytes: u64,
) -> Result<Option<String>, std::io::Error> {
    if file_size_bytes > INLINE_CONTENT_HASH_LIMIT_BYTES {
        return Ok(None);
    }

    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(Some(hex_lower(&hasher.finalize())))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }

    output
}

/// Read the embedded tag observation and the track duration in one probe. Properties
/// are parsed so we get the stream length; a zero/unknown duration becomes `None`.
fn read_embedded_metadata(
    path: &Path,
    scan_errors: &mut Vec<ScanIssue>,
    observed_at_unix_seconds: i64,
) -> (Option<TrackMetadataObservation>, Option<f64>) {
    // Prefer reading stream properties (so we get a duration), but some files — and
    // minimal/edge-case inputs — fail property parsing while their tags are still
    // readable. Fall back to a tags-only read in that case (duration stays `None`).
    let with_properties = Probe::open(path)
        .map(|probe| probe.options(ParseOptions::new().read_properties(true)))
        .and_then(Probe::read);
    let (tagged_file, duration) = match with_properties {
        Ok(tagged_file) => {
            let seconds = tagged_file.properties().duration().as_secs_f64();
            (tagged_file, (seconds > 0.0).then_some(seconds))
        }
        Err(_) => {
            match Probe::open(path)
                .map(|probe| probe.options(ParseOptions::new().read_properties(false)))
                .and_then(Probe::read)
            {
                Ok(tagged_file) => (tagged_file, None),
                Err(error) => {
                    scan_errors.push(ScanIssue {
                        path: path.display().to_string(),
                        message: format!("metadata read failed: {error}"),
                    });
                    return (None, None);
                }
            }
        }
    };

    let observation = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .and_then(|tag| TrackMetadataObservation::embedded_tag(tag, observed_at_unix_seconds));
    (observation, duration)
}

fn read_sidecar_lyrics(
    path: &Path,
    scan_errors: &mut Vec<ScanIssue>,
    observed_at_unix_seconds: i64,
) -> Option<TrackMetadataObservation> {
    for (extension, source, confidence) in LYRIC_SIDECARS {
        let sidecar_path = path.with_extension(extension);
        if !sidecar_path.exists() {
            continue;
        }

        match fs::read_to_string(&sidecar_path) {
            Ok(contents) => {
                if let Some(lyrics) = clean_optional_lyrics_value(Some(&contents)) {
                    return Some(TrackMetadataObservation::sidecar_lyrics(
                        source,
                        *confidence,
                        lyrics,
                        observed_at_unix_seconds,
                    ));
                }
            }
            Err(source) => scan_errors.push(ScanIssue {
                path: sidecar_path.display().to_string(),
                message: format!("sidecar lyrics read failed: {source}"),
            }),
        }
    }

    None
}

fn metadata_observations(
    embedded_metadata: Option<TrackMetadataObservation>,
    sidecar_lyrics: Option<TrackMetadataObservation>,
    folder_metadata: &TrackMetadata,
    observed_at_unix_seconds: i64,
) -> Vec<TrackMetadataObservation> {
    let mut observations = Vec::new();

    if let Some(embedded_metadata) = embedded_metadata {
        observations.push(embedded_metadata);
    }

    if let Some(sidecar_lyrics) = sidecar_lyrics {
        observations.push(sidecar_lyrics);
    }

    observations.push(TrackMetadataObservation::folder_path(
        folder_metadata,
        observed_at_unix_seconds,
    ));
    observations
}

fn canonical_metadata(
    embedded_metadata: Option<&TrackMetadataObservation>,
    folder_metadata: &TrackMetadata,
) -> TrackMetadata {
    TrackMetadata {
        title: embedded_metadata
            .and_then(|metadata| metadata.title.clone())
            .unwrap_or_else(|| folder_metadata.title.clone()),
        artist_name: embedded_metadata
            .and_then(|metadata| metadata.artist_name.clone())
            .unwrap_or_else(|| folder_metadata.artist_name.clone()),
        album_artist_name: embedded_metadata
            .and_then(|metadata| metadata.album_artist_name.clone())
            .or_else(|| folder_metadata.album_artist_name.clone()),
        album_title: embedded_metadata
            .and_then(|metadata| metadata.album_title.clone())
            .unwrap_or_else(|| folder_metadata.album_title.clone()),
        year: embedded_metadata
            .and_then(|metadata| metadata.year)
            .or(folder_metadata.year),
        track_number: embedded_metadata
            .and_then(|metadata| metadata.track_number)
            .or(folder_metadata.track_number),
        disc_number: embedded_metadata
            .and_then(|metadata| metadata.disc_number)
            .or(folder_metadata.disc_number),
    }
}

fn infer_track_metadata(root: &Path, path: &Path) -> TrackMetadata {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled")
        .trim();
    let parent = path.parent().unwrap_or(root);
    let album_dir = parent
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown Album");

    let (year, album_title) = parse_album_dir(album_dir);
    let (track_number, title_stem) = parse_track_number(stem);
    let (artist_name, title) = parse_artist_title(title_stem);

    TrackMetadata {
        title: clean_title(title),
        artist_name: clean_title(artist_name),
        album_artist_name: None,
        album_title,
        year,
        track_number,
        disc_number: None,
    }
}

fn parse_album_dir(album_dir: &str) -> (Option<u16>, String) {
    if let Some((year, title)) = album_dir.split_once(" - ") {
        if year.len() == 4 && year.chars().all(|char| char.is_ascii_digit()) {
            return (year.parse().ok(), clean_title(title));
        }
    }

    (None, clean_title(album_dir))
}

fn parse_track_number(stem: &str) -> (Option<u16>, &str) {
    let digits_len = stem
        .chars()
        .take_while(|char| char.is_ascii_digit())
        .map(char::len_utf8)
        .sum();

    if digits_len == 0 || digits_len > 3 {
        return (None, stem);
    }

    let number = stem[..digits_len].parse().ok();
    let rest = stem[digits_len..]
        .trim_start_matches([' ', '.', '-', '_'])
        .trim();

    if rest.is_empty() {
        (None, stem)
    } else {
        (number, rest)
    }
}

fn parse_artist_title(stem: &str) -> (&str, &str) {
    stem.split_once(" - ")
        .map(|(artist, title)| (artist.trim(), title.trim()))
        .unwrap_or(("Unknown Artist", stem.trim()))
}

fn clean_title(value: &str) -> String {
    let cleaned = value.trim().replace('_', " ");
    if cleaned.is_empty() {
        "Unknown".to_string()
    } else {
        cleaned
    }
}

fn clean_optional_tag_value(value: Option<&str>) -> Option<String> {
    value
        .map(|value| value.trim().replace('_', " "))
        .filter(|value| !value.is_empty())
}

fn clean_tag_value(tag: &lofty::tag::Tag, key: ItemKey) -> Option<String> {
    clean_optional_tag_value(tag.get_string(key))
}

fn clean_tag_lyrics_value(tag: &lofty::tag::Tag, key: ItemKey) -> Option<String> {
    clean_optional_lyrics_value(tag.get_string(key))
}

fn first_clean_tag_value(tag: &lofty::tag::Tag, key: ItemKey) -> Option<String> {
    clean_tag_values(tag.get_strings(key)).into_iter().next()
}

fn clean_tag_values<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut cleaned = Vec::new();

    for value in values.filter_map(|value| clean_optional_tag_value(Some(value))) {
        if !cleaned.contains(&value) {
            cleaned.push(value);
        }
    }

    cleaned
}

fn clean_optional_lyrics_value(value: Option<&str>) -> Option<String> {
    value
        .map(|value| value.replace("\r\n", "\n").replace('\r', "\n"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn u32_to_u16(value: u32) -> Option<u16> {
    u16::try_from(value).ok()
}

pub fn album_artwork_url(album_id: &str, path: &Path) -> String {
    format!(
        "/api/albums/{album_id}/artwork?asset={}",
        artwork_asset_id(path)
    )
}

pub fn artwork_asset_id(path: &Path) -> String {
    let mut identity = path.to_string_lossy().to_string();

    if let Ok(metadata) = fs::metadata(path) {
        identity.push('|');
        identity.push_str(&metadata.len().to_string());
        identity.push('|');
        identity.push_str(
            &metadata
                .modified()
                .ok()
                .and_then(system_time_to_unix_seconds)
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
    }

    stable_id("artwork", &identity)
}

pub fn find_album_artwork_candidates(album_dir: &Path) -> Vec<PathBuf> {
    let Some(entries) = fs::read_dir(album_dir).ok() else {
        return Vec::new();
    };
    let mut images = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && has_extension(&path, IMAGE_EXTENSIONS) {
            images.push(path);
        }
    }

    images.sort_by(|left, right| {
        artwork_priority(left)
            .cmp(&artwork_priority(right))
            .then_with(|| left.cmp(right))
    });

    images
}

fn find_album_artwork(album_dir: &Path) -> Option<PathBuf> {
    find_album_artwork_candidates(album_dir).into_iter().next()
}

fn artwork_priority(path: &Path) -> u8 {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "cover" => 0,
        "folder" => 1,
        "front" => 2,
        _ => 3,
    }
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
        .unwrap_or(false)
}

fn relative_display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_track_identity(
    metadata: &TrackMetadata,
    extension: &str,
    file_size_bytes: Option<u64>,
    modified_at_unix_seconds: Option<i64>,
    content_hash: Option<&str>,
) -> String {
    let source_identity = content_hash
        .map(|hash| format!("sha256:{hash}"))
        .unwrap_or_else(|| {
            format!(
                "unhashed:size={}:modified={}",
                file_size_bytes
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                modified_at_unix_seconds
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            )
        });

    format!(
        "{}::artist={}::album={}::title={}::year={}::track={}::extension={}",
        source_identity,
        metadata.artist_name.to_ascii_lowercase(),
        metadata.album_title.to_ascii_lowercase(),
        metadata.title.to_ascii_lowercase(),
        metadata
            .year
            .map(|value| value.to_string())
            .unwrap_or_default(),
        metadata
            .track_number
            .map(|value| value.to_string())
            .unwrap_or_default(),
        extension
    )
}

fn unique_track_id(identity: &str, counts: &mut BTreeMap<String, usize>) -> String {
    let count = counts.entry(identity.to_string()).or_default();
    let id = if *count == 0 {
        stable_id("track", identity)
    } else {
        stable_id("track", &format!("{identity}::duplicate={count}"))
    };
    *count += 1;
    id
}

/// The directory portion of a track's library-relative path, or `None` for a
/// file that sits directly in the library root.
fn track_folder(track: &Track) -> Option<&str> {
    track
        .relative_path
        .rsplit_once('/')
        .map(|(folder, _file)| folder)
}

/// Matches a track when the requested folder equals its folder or is an ancestor
/// of it (so browsing a parent folder includes nested folders).
fn folder_filter_matches(filter: Option<&str>, track: &Track) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) else {
        return true;
    };
    let filter = filter.trim_end_matches('/');
    match track_folder(track) {
        Some(folder) => folder == filter || folder.starts_with(&format!("{filter}/")),
        None => false,
    }
}

fn text_filter_matches(filter: Option<&str>, values: BTreeSet<String>) -> bool {
    let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) else {
        return true;
    };

    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(filter))
}

fn track_genres(track: &Track) -> BTreeSet<String> {
    track
        .observed_metadata
        .iter()
        .flat_map(|observation| observation.genres.iter())
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect()
}

fn track_composers(track: &Track) -> BTreeSet<String> {
    track
        .observed_metadata
        .iter()
        .flat_map(|observation| observation.composers.iter())
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect()
}

fn stable_id(prefix: &str, value: &str) -> String {
    let mut hasher = StableHasher::default();
    value.hash(&mut hasher);
    format!("{prefix}_{:016x}", hasher.finish())
}

#[derive(Default)]
struct StableHasher(u64);

impl Hasher for StableHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };

        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }

        self.0 = hash;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::{
        config::WriteOptions,
        picture::{MimeType, Picture, PictureType},
        prelude::TagExt,
        tag::{ItemKey, ItemValue, Tag, TagItem, TagType},
    };
    use std::{fs, time::SystemTime};

    #[test]
    fn scans_local_library_fixture() {
        let fixture = TestFixture::new("scan");
        fixture.write("1994 - Paramparcad/Darkwood Dub - Brzi Vavilon.mp3");
        fixture.write("1994 - Paramparcad/Darkwood Dub - Spori Vavilon.mp3");
        fixture.write("1994 - Paramparcad/cover.jpg");
        fixture.write("1996 - U Nedogled/Darkwood Dub - U Nedogled.mp3");
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        assert_eq!(library.tracks.len(), 3);
        assert_eq!(library.albums.len(), 2);
        assert!(
            library
                .artists
                .iter()
                .any(|artist| artist.name == "Darkwood Dub")
        );
        assert!(
            library
                .tracks
                .iter()
                .all(|track| track.provider.provider_id == "local-disk")
        );
        assert!(
            library
                .albums
                .iter()
                .any(|album| album.artwork_url.is_some())
        );
        assert!(
            library
                .albums
                .iter()
                .filter_map(|album| album.artwork_url.as_deref())
                .any(|url| url.contains("?asset=artwork_"))
        );
        assert!(!library.scan_errors.is_empty());
        assert!(
            library
                .scan_errors
                .iter()
                .all(|issue| issue.message.contains("metadata read failed"))
        );
        assert!(
            library
                .tracks
                .iter()
                .all(|track| track.file_size_bytes == Some(0))
        );
        assert!(library.tracks.iter().all(|track| {
            track.content_hash.as_deref()
                == Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        }));
        assert!(library.tracks.iter().all(|track| {
            track
                .observed_metadata
                .iter()
                .any(|observation| observation.source == "folder_path")
        }));
    }

    #[test]
    fn finds_album_artwork_candidates_in_priority_order() {
        let fixture = TestFixture::new("artwork-candidates");
        fixture.write("Album/back.jpg");
        fixture.write("Album/front.png");
        fixture.write("Album/folder.webp");
        fixture.write("Album/cover.jpg");

        let names = find_album_artwork_candidates(&fixture.root.join("Album"))
            .into_iter()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["cover.jpg", "folder.webp", "front.png", "back.jpg"]
        );
    }

    #[test]
    fn browse_index_and_filters_metadata_facets() {
        let fixture = TestFixture::new("browse");
        fixture.write_rich_tagged_mp3("2004 - Rich Album/Rich Artist - Rich Title.mp3");
        fixture.write_tagged_mp3(
            "1994 - Plain Album/Plain Artist - Plain Title.mp3",
            TaggedFixtureMetadata {
                title: "Plain Title",
                artist: "Plain Artist",
                album: "Plain Album",
                year: 1994,
                track_number: 1,
            },
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let index = library.browse_index();

        assert!(
            index
                .genres
                .iter()
                .any(|facet| { facet.value == "Dub" && facet.track_count == 1 })
        );
        assert!(
            index
                .composers
                .iter()
                .any(|facet| { facet.value == "Composer Two" && facet.track_count == 1 })
        );
        assert!(
            index
                .years
                .iter()
                .any(|facet| { facet.value == 2004 && facet.track_count == 1 })
        );

        let by_genre = library.browse_tracks(&BrowseFilter {
            genre: Some("dub".to_string()),
            ..BrowseFilter::default()
        });
        assert_eq!(by_genre.len(), 1);
        assert_eq!(by_genre[0].title, "Rich Title");

        let by_year = library.browse_tracks(&BrowseFilter {
            year: Some(1994),
            ..BrowseFilter::default()
        });
        assert_eq!(by_year.len(), 1);
        assert_eq!(by_year[0].title, "Plain Title");

        let no_match = library.browse_tracks(&BrowseFilter {
            year: Some(1994),
            composer: Some("Composer Two".to_string()),
            ..BrowseFilter::default()
        });
        assert!(no_match.is_empty());
    }

    #[test]
    fn canonical_track_id_uses_content_identity() {
        let fixture = TestFixture::new("identity");
        let path = "1994 - Paramparcad/Darkwood Dub - Brzi Vavilon.mp3";
        fixture.write_bytes(path, b"first version");
        let first = scan_local_library(&fixture.root).expect("scan fixture");
        let first_track = first.tracks.first().expect("first track");

        fixture.write_bytes(path, b"second version");
        let second = scan_local_library(&fixture.root).expect("scan fixture");
        let second_track = second.tracks.first().expect("second track");

        assert_eq!(first_track.provider.item_id, second_track.provider.item_id);
        assert_ne!(first_track.id, second_track.id);
    }

    #[test]
    fn skips_inline_content_hash_for_large_files() {
        let fixture = TestFixture::new("large-hash");
        let contents = vec![0_u8; INLINE_CONTENT_HASH_LIMIT_BYTES as usize + 1];
        fixture.write_bytes(
            "1994 - Paramparcad/Darkwood Dub - Brzi Vavilon.mp3",
            &contents,
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");

        assert_eq!(track.file_size_bytes, Some(contents.len() as u64));
        assert_eq!(track.content_hash, None);
    }

    #[test]
    fn canonical_metadata_prefers_embedded_tags_with_folder_fallbacks() {
        let folder = TrackMetadata {
            title: "Folder Title".to_string(),
            artist_name: "Folder Artist".to_string(),
            album_artist_name: None,
            album_title: "Folder Album".to_string(),
            year: Some(1994),
            track_number: Some(7),
            disc_number: None,
        };
        let embedded = TrackMetadataObservation {
            source: "embedded_tag".to_string(),
            confidence: 0.95,
            observed_at_unix_seconds: 1_800_000_000,
            approval_state: MetadataApprovalState::Observed,
            field_observations: Vec::new(),
            title: Some("Embedded Title".to_string()),
            artist_name: None,
            album_artist_name: None,
            album_title: Some("Embedded Album".to_string()),
            recording_date: None,
            year: None,
            track_number: Some(2),
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
        };

        let canonical = canonical_metadata(Some(&embedded), &folder);

        assert_eq!(canonical.title, "Embedded Title");
        assert_eq!(canonical.artist_name, "Folder Artist");
        assert_eq!(canonical.album_title, "Embedded Album");
        assert_eq!(canonical.year, Some(1994));
        assert_eq!(canonical.track_number, Some(2));
    }

    #[test]
    fn scans_embedded_mp3_metadata_fixture() {
        let fixture = TestFixture::new("embedded-mp3");
        fixture.write_tagged_mp3(
            "1994 - Folder Album/Folder Artist - Folder Title.mp3",
            TaggedFixtureMetadata {
                title: "Embedded Title",
                artist: "Embedded Artist",
                album: "Embedded Album",
                year: 2001,
                track_number: 9,
            },
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");

        assert_eq!(track.title, "Embedded Title");
        assert_eq!(track.artist_name, "Embedded Artist");
        assert_eq!(track.album_title, "Embedded Album");
        assert_eq!(track.year, Some(2001));
        assert_eq!(track.track_number, Some(9));
        assert!(
            track
                .observed_metadata
                .iter()
                .any(|observation| observation.source == "embedded_tag"
                    && observation.confidence == 0.95)
        );
        assert!(
            track
                .observed_metadata
                .iter()
                .any(|observation| observation.source == "folder_path"
                    && observation.confidence == 0.55)
        );
    }

    #[test]
    fn partial_embedded_tags_keep_folder_fallbacks() {
        let fixture = TestFixture::new("partial-embedded");
        fixture
            .write_partially_tagged_mp3("1994 - Folder Album/07 Folder Artist - Folder Title.mp3");
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");
        let embedded_observation = track
            .observed_metadata
            .iter()
            .find(|observation| observation.source == "embedded_tag")
            .expect("embedded observation");

        assert_eq!(track.title, "Only Embedded Title");
        assert_eq!(track.artist_name, "Folder Artist");
        assert_eq!(track.album_title, "Folder Album");
        assert_eq!(track.year, Some(1994));
        assert_eq!(track.track_number, Some(7));
        assert_eq!(
            embedded_observation.title,
            Some("Only Embedded Title".to_string())
        );
        assert_eq!(embedded_observation.artist_name, None);
        assert_eq!(embedded_observation.album_title, None);
    }

    #[test]
    fn scans_embedded_flac_metadata_fixture() {
        let fixture = TestFixture::new("embedded-flac");
        fixture.write_tagged_flac(
            "1994 - Folder Album/Folder Artist - Folder Title.flac",
            TaggedFixtureMetadata {
                title: "FLAC Title",
                artist: "FLAC Artist",
                album: "FLAC Album",
                year: 2002,
                track_number: 4,
            },
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");

        assert_eq!(track.title, "FLAC Title");
        assert_eq!(track.artist_name, "FLAC Artist");
        assert_eq!(track.album_title, "FLAC Album");
        assert_eq!(track.year, Some(2002));
        assert_eq!(track.track_number, Some(4));
        assert!(
            track
                .observed_metadata
                .iter()
                .any(|observation| observation.source == "embedded_tag")
        );
    }

    #[test]
    fn scans_embedded_m4a_metadata_fixture() {
        let fixture = TestFixture::new("embedded-m4a");
        fixture.write_tagged_m4a(
            "1994 - Folder Album/Folder Artist - Folder Title.m4a",
            TaggedFixtureMetadata {
                title: "M4A Title",
                artist: "M4A Artist",
                album: "M4A Album",
                year: 2003,
                track_number: 5,
            },
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");

        assert_eq!(track.title, "M4A Title");
        assert_eq!(track.artist_name, "M4A Artist");
        assert_eq!(track.album_title, "M4A Album");
        assert_eq!(track.year, Some(2003));
        assert_eq!(track.track_number, Some(5));
        assert!(
            track
                .observed_metadata
                .iter()
                .any(|observation| observation.source == "embedded_tag")
        );
    }

    #[test]
    fn scans_rich_embedded_metadata_fixture() {
        let fixture = TestFixture::new("rich-embedded");
        fixture.write_rich_tagged_mp3("1994 - Folder Album/Folder Artist - Folder Title.mp3");
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");
        let observation = track
            .observed_metadata
            .iter()
            .find(|observation| observation.source == "embedded_tag")
            .expect("embedded observation");

        assert_eq!(
            observation.album_artist_name,
            Some("Various Artists".to_string())
        );
        assert_eq!(observation.recording_date, Some("2004-05-06".to_string()));
        assert_eq!(observation.track_total, Some(11));
        assert_eq!(observation.disc_number, Some(2));
        assert_eq!(observation.disc_total, Some(3));
        assert_eq!(observation.genres, vec!["Dub", "Electronic"]);
        assert_eq!(observation.composers, vec!["Composer One", "Composer Two"]);
        assert_eq!(observation.lyrics, Some("Line one\nLine two".to_string()));
        assert_eq!(
            observation.musicbrainz_recording_id,
            Some("recording-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_artist_id,
            Some("artist-id".to_string())
        );
        assert_eq!(observation.isrc, Some("USRC17607839".to_string()));
        assert_eq!(observation.embedded_artwork_count, 1);
        assert!(observation.field_observations.iter().any(|field| {
            field.field_name == "genres"
                && field.value
                    == MetadataFieldValue::TextList(vec![
                        "Dub".to_string(),
                        "Electronic".to_string(),
                    ])
        }));
        assert!(observation.field_observations.iter().all(|field| {
            field.source == "embedded_tag"
                && field.confidence == 0.95
                && field.approval_state == MetadataApprovalState::Observed
        }));
    }

    #[test]
    fn groups_compilation_tracks_under_album_artist() {
        let fixture = TestFixture::new("compilation");
        fixture.write_album_grouping_mp3(
            "Comp/01 First.mp3",
            "First",
            "Artist One",
            "Best Of",
            Some("Various Artists"),
            None,
            1,
        );
        fixture.write_album_grouping_mp3(
            "Comp/02 Second.mp3",
            "Second",
            "Artist Two",
            "Best Of",
            Some("Various Artists"),
            None,
            2,
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        // Both tracks collapse into a single compilation album owned by the album-artist.
        assert_eq!(library.albums.len(), 1);
        let album = library.albums.first().expect("album");
        assert_eq!(album.artist_name, "Various Artists");
        assert_eq!(album.track_count, 2);

        // Each track keeps its own (track) artist.
        let mut track_artists: Vec<_> = library
            .tracks
            .iter()
            .map(|track| track.artist_name.as_str())
            .collect();
        track_artists.sort();
        assert_eq!(track_artists, vec!["Artist One", "Artist Two"]);
        assert!(
            library
                .tracks
                .iter()
                .all(|track| track.album_id == album.id)
        );

        // The album-artist is registered and owns the compilation.
        let various = library
            .artists
            .iter()
            .find(|artist| artist.name == "Various Artists")
            .expect("album-artist registered");
        assert_eq!(various.album_count, 1);
    }

    #[test]
    fn orders_multi_disc_album_tracks_by_disc_then_track() {
        let fixture = TestFixture::new("multi-disc");
        fixture.write_album_grouping_mp3(
            "Set/d2t1.mp3",
            "Disc Two Opener",
            "One Artist",
            "Anthology",
            None,
            Some(2),
            1,
        );
        fixture.write_album_grouping_mp3(
            "Set/d1t1.mp3",
            "Disc One Opener",
            "One Artist",
            "Anthology",
            None,
            Some(1),
            1,
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        // Multi-disc release stays a single album.
        assert_eq!(library.albums.len(), 1);
        assert_eq!(library.tracks.len(), 2);
        assert!(
            library
                .tracks
                .iter()
                .any(|track| track.disc_number == Some(1))
        );

        // Global ordering places disc 1 before disc 2 despite identical track numbers.
        let ordered: Vec<_> = library
            .tracks
            .iter()
            .map(|track| (track.disc_number, track.title.as_str()))
            .collect();
        assert_eq!(
            ordered,
            vec![(Some(1), "Disc One Opener"), (Some(2), "Disc Two Opener")]
        );
    }

    #[test]
    fn browses_by_folder_facet_and_filter() {
        let fixture = TestFixture::new("folder-browse");
        fixture.write_album_grouping_mp3(
            "Rock/track.mp3",
            "Rock Song",
            "Artist",
            "Rock Album",
            None,
            None,
            1,
        );
        fixture.write_album_grouping_mp3(
            "Jazz/track.mp3",
            "Jazz Song",
            "Artist",
            "Jazz Album",
            None,
            None,
            1,
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        let folders: Vec<_> = library
            .browse_index()
            .folders
            .into_iter()
            .map(|facet| facet.value)
            .collect();
        assert!(folders.contains(&"Rock".to_string()));
        assert!(folders.contains(&"Jazz".to_string()));

        let filter = BrowseFilter {
            folder: Some("Rock".to_string()),
            ..BrowseFilter::default()
        };
        let tracks = library.browse_tracks(&filter);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Rock Song");
    }

    #[test]
    fn scans_lrc_sidecar_lyrics_fixture() {
        let fixture = TestFixture::new("sidecar-lrc");
        fixture.write_tagged_mp3(
            "1994 - Folder Album/01 Folder Artist - Folder Title.mp3",
            TaggedFixtureMetadata {
                title: "Tagged Title",
                artist: "Tagged Artist",
                album: "Tagged Album",
                year: 1994,
                track_number: 1,
            },
        );
        fixture.write_bytes(
            "1994 - Folder Album/01 Folder Artist - Folder Title.lrc",
            b"\r\n[00:01.00]First_line\r\n[00:02.00]Second line\r\n",
        );
        fixture.write_bytes(
            "1994 - Folder Album/01 Folder Artist - Folder Title.txt",
            b"Plain fallback",
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");
        let observation = track
            .observed_metadata
            .iter()
            .find(|observation| observation.source == "sidecar_lrc")
            .expect("sidecar observation");

        assert_eq!(
            observation.lyrics,
            Some("[00:01.00]First_line\n[00:02.00]Second line".to_string())
        );
        assert_eq!(observation.confidence, 0.9);
        assert!(
            track
                .observed_metadata
                .iter()
                .all(|observation| observation.source != "sidecar_txt")
        );
        assert!(observation.field_observations.iter().any(|field| {
            field.field_name == "lyrics"
                && field.value
                    == MetadataFieldValue::Text(
                        "[00:01.00]First_line\n[00:02.00]Second line".to_string(),
                    )
                && field.source == "sidecar_lrc"
        }));
    }

    #[test]
    fn scans_txt_sidecar_lyrics_fixture() {
        let fixture = TestFixture::new("sidecar-txt");
        fixture.write_tagged_mp3(
            "1994 - Folder Album/02 Folder Artist - Folder Title.mp3",
            TaggedFixtureMetadata {
                title: "Tagged Title",
                artist: "Tagged Artist",
                album: "Tagged Album",
                year: 1994,
                track_number: 2,
            },
        );
        fixture.write_bytes(
            "1994 - Folder Album/02 Folder Artist - Folder Title.txt",
            b"Plain line one\rPlain_line_two\n",
        );
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let track = library.tracks.first().expect("track");
        let observation = track
            .observed_metadata
            .iter()
            .find(|observation| observation.source == "sidecar_txt")
            .expect("sidecar observation");

        assert_eq!(
            observation.lyrics,
            Some("Plain line one\nPlain_line_two".to_string())
        );
        assert_eq!(observation.confidence, 0.8);
    }

    #[test]
    fn embedded_tag_extracts_external_identifier_fields() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzRecordingId,
            ItemValue::Text("recording-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzTrackId,
            ItemValue::Text("track-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzReleaseId,
            ItemValue::Text("release-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzReleaseGroupId,
            ItemValue::Text("release-group-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzArtistId,
            ItemValue::Text("artist-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::MusicBrainzReleaseArtistId,
            ItemValue::Text("release-artist-id".to_string()),
        ));
        tag.push_unchecked(TagItem::new(
            ItemKey::Isrc,
            ItemValue::Text("USRC17607839".to_string()),
        ));

        let observation =
            TrackMetadataObservation::embedded_tag(&tag, 1_800_000_000).expect("observation");

        assert_eq!(
            observation.musicbrainz_recording_id,
            Some("recording-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_track_id,
            Some("track-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_release_id,
            Some("release-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_release_group_id,
            Some("release-group-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_artist_id,
            Some("artist-id".to_string())
        );
        assert_eq!(
            observation.musicbrainz_release_artist_id,
            Some("release-artist-id".to_string())
        );
        assert_eq!(observation.isrc, Some("USRC17607839".to_string()));
    }

    struct TaggedFixtureMetadata {
        title: &'static str,
        artist: &'static str,
        album: &'static str,
        year: u16,
        track_number: u16,
    }

    struct TestFixture {
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("musicata-core-{name}-{unique}"));
            fs::create_dir_all(&root).expect("create fixture root");
            Self { root }
        }

        fn write(&self, relative_path: &str) {
            self.write_bytes(relative_path, &[]);
        }

        fn write_bytes(&self, relative_path: &str, contents: &[u8]) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");
            fs::write(path, contents).expect("write fixture file");
        }

        fn write_tagged_mp3(&self, relative_path: &str, metadata: TaggedFixtureMetadata) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");

            let mut tag = Tag::new(TagType::Id3v2);
            tag.insert_text(ItemKey::TrackTitle, metadata.title.to_string());
            tag.insert_text(ItemKey::TrackArtist, metadata.artist.to_string());
            tag.insert_text(ItemKey::AlbumTitle, metadata.album.to_string());
            tag.insert_text(ItemKey::RecordingDate, metadata.year.to_string());
            tag.insert_text(ItemKey::TrackNumber, metadata.track_number.to_string());

            let mut bytes = Vec::new();
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("write fixture tag");
            bytes.extend_from_slice(&minimal_mpeg_frame());
            fs::write(path, bytes).expect("write fixture file");
        }

        fn write_partially_tagged_mp3(&self, relative_path: &str) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");

            let mut tag = Tag::new(TagType::Id3v2);
            tag.set_title("Only Embedded Title".to_string());

            let mut bytes = Vec::new();
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("write id3v2 tag");
            bytes.extend_from_slice(&minimal_mpeg_frame());
            fs::write(path, bytes).expect("write fixture file");
        }

        fn write_tagged_flac(&self, relative_path: &str, metadata: TaggedFixtureMetadata) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");
            fs::write(path, tagged_flac_bytes(metadata)).expect("write fixture file");
        }

        fn write_tagged_m4a(&self, relative_path: &str, metadata: TaggedFixtureMetadata) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");
            fs::write(path, tagged_m4a_bytes(metadata)).expect("write fixture file");
        }

        fn write_rich_tagged_mp3(&self, relative_path: &str) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");

            let mut tag = Tag::new(TagType::Id3v2);
            tag.set_title("Rich Title".to_string());
            tag.set_artist("Rich Artist".to_string());
            tag.set_album("Rich Album".to_string());
            tag.insert_text(ItemKey::AlbumArtist, "Various Artists".to_string());
            tag.insert_text(ItemKey::RecordingDate, "2004-05-06".to_string());
            tag.set_track(6);
            tag.set_track_total(11);
            tag.set_disk(2);
            tag.set_disk_total(3);
            tag.push(TagItem::new(
                ItemKey::Genre,
                ItemValue::Text("Dub".to_string()),
            ));
            tag.push(TagItem::new(
                ItemKey::Genre,
                ItemValue::Text("Electronic".to_string()),
            ));
            tag.push(TagItem::new(
                ItemKey::Composer,
                ItemValue::Text("Composer One".to_string()),
            ));
            tag.push(TagItem::new(
                ItemKey::Composer,
                ItemValue::Text("Composer Two".to_string()),
            ));
            tag.insert_text(ItemKey::UnsyncLyrics, "Line one\nLine two".to_string());
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzRecordingId,
                ItemValue::Text("recording-id".to_string()),
            ));
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzTrackId,
                ItemValue::Text("track-id".to_string()),
            ));
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzReleaseId,
                ItemValue::Text("release-id".to_string()),
            ));
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzReleaseGroupId,
                ItemValue::Text("release-group-id".to_string()),
            ));
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzArtistId,
                ItemValue::Text("artist-id".to_string()),
            ));
            tag.push_unchecked(TagItem::new(
                ItemKey::MusicBrainzReleaseArtistId,
                ItemValue::Text("release-artist-id".to_string()),
            ));
            tag.insert_text(ItemKey::Isrc, "USRC17607839".to_string());
            tag.push_picture(
                Picture::unchecked(vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0])
                    .pic_type(PictureType::CoverFront)
                    .mime_type(MimeType::Jpeg)
                    .build(),
            );

            let mut bytes = Vec::new();
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("write id3v2 tag");
            bytes.extend_from_slice(&minimal_mpeg_frame());
            fs::write(path, bytes).expect("write fixture file");
        }

        #[allow(clippy::too_many_arguments)]
        fn write_album_grouping_mp3(
            &self,
            relative_path: &str,
            title: &str,
            artist: &str,
            album: &str,
            album_artist: Option<&str>,
            disc_number: Option<u32>,
            track_number: u32,
        ) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");

            let mut tag = Tag::new(TagType::Id3v2);
            tag.set_title(title.to_string());
            tag.set_artist(artist.to_string());
            tag.set_album(album.to_string());
            if let Some(album_artist) = album_artist {
                tag.insert_text(ItemKey::AlbumArtist, album_artist.to_string());
            }
            if let Some(disc_number) = disc_number {
                tag.set_disk(disc_number);
            }
            tag.set_track(track_number);

            let mut bytes = Vec::new();
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("write id3v2 tag");
            bytes.extend_from_slice(&minimal_mpeg_frame());
            fs::write(path, bytes).expect("write fixture file");
        }
    }

    fn tagged_flac_bytes(metadata: TaggedFixtureMetadata) -> Vec<u8> {
        let comments = vorbis_comment_block([
            ("TITLE", metadata.title.to_string()),
            ("ARTIST", metadata.artist.to_string()),
            ("ALBUM", metadata.album.to_string()),
            ("DATE", metadata.year.to_string()),
            ("TRACKNUMBER", metadata.track_number.to_string()),
        ]);
        let mut bytes = Vec::new();

        bytes.extend_from_slice(b"fLaC");
        bytes.push(0x00);
        bytes.extend_from_slice(&34_u32.to_be_bytes()[1..]);
        bytes.extend_from_slice(&[0_u8; 34]);
        bytes.push(0x80 | 0x04);
        bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
        bytes.extend_from_slice(&comments);

        bytes
    }

    fn tagged_m4a_bytes(metadata: TaggedFixtureMetadata) -> Vec<u8> {
        let ilst = mp4_atom(
            b"ilst",
            [
                mp4_ilst_text_atom(b"\xA9nam", metadata.title),
                mp4_ilst_text_atom(b"\xA9ART", metadata.artist),
                mp4_ilst_text_atom(b"\xA9alb", metadata.album),
                mp4_ilst_text_atom(b"\xA9day", &metadata.year.to_string()),
                mp4_track_number_atom(metadata.track_number),
            ]
            .concat(),
        );

        let hdlr = mp4_atom(
            b"hdlr",
            [
                [0_u8; 8].as_slice(),
                b"mdirappl".as_slice(),
                [0_u8; 9].as_slice(),
            ]
            .concat(),
        );

        let meta = mp4_atom(
            b"meta",
            [[0_u8; 4].as_slice(), hdlr.as_slice(), ilst.as_slice()].concat(),
        );
        let udta = mp4_atom(b"udta", meta);
        let moov = mp4_atom(b"moov", udta);
        let ftyp = mp4_atom(b"ftyp", [b"M4A ".as_slice(), [0_u8; 4].as_slice()].concat());

        [ftyp, moov].concat()
    }

    fn mp4_ilst_text_atom(fourcc: &[u8; 4], value: &str) -> Vec<u8> {
        mp4_atom(fourcc, mp4_data_atom(1, value.as_bytes()))
    }

    fn mp4_track_number_atom(track_number: u16) -> Vec<u8> {
        let mut content = Vec::new();
        content.extend_from_slice(&[0_u8; 2]);
        content.extend_from_slice(&track_number.to_be_bytes());
        content.extend_from_slice(&[0_u8; 4]);

        mp4_atom(b"trkn", mp4_data_atom(0, &content))
    }

    fn mp4_data_atom(data_type: u32, content: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(0);
        payload.extend_from_slice(&data_type.to_be_bytes()[1..]);
        payload.extend_from_slice(&[0_u8; 4]);
        payload.extend_from_slice(content);

        mp4_atom(b"data", payload)
    }

    fn mp4_atom(fourcc: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let size = payload.len() as u32 + 8;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(fourcc);
        bytes.extend_from_slice(&payload);

        bytes
    }

    fn vorbis_comment_block<const N: usize>(comments: [(&str, String); N]) -> Vec<u8> {
        let vendor = b"musicata-test";
        let mut bytes = Vec::new();

        bytes.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        bytes.extend_from_slice(vendor);
        bytes.extend_from_slice(&(comments.len() as u32).to_le_bytes());

        for (key, value) in comments {
            let comment = format!("{key}={value}");
            bytes.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            bytes.extend_from_slice(comment.as_bytes());
        }

        bytes
    }

    fn minimal_mpeg_frame() -> [u8; 417] {
        let mut frame = [0_u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        frame
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
