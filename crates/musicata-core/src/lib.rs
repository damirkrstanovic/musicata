//! Core domain and provider interfaces for Musicata.
//!
//! The initial implementation ships a local-disk provider, but the domain model
//! deliberately describes music independently from the source that provided it.

use lofty::{
    config::ParseOptions, file::TaggedFileExt, prelude::Accessor, probe::Probe, tag::ItemKey,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display},
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

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

    pub fn search(&self, query: &str) -> SearchResults {
        let needle = query.trim().to_ascii_lowercase();

        if needle.is_empty() {
            return SearchResults::default();
        }

        let artists = self
            .artists
            .iter()
            .filter(|artist| contains_ascii_case_insensitive(&artist.name, &needle))
            .cloned()
            .collect();

        let albums = self
            .albums
            .iter()
            .filter(|album| {
                contains_ascii_case_insensitive(&album.title, &needle)
                    || contains_ascii_case_insensitive(&album.artist_name, &needle)
            })
            .cloned()
            .collect();

        let tracks = self
            .tracks
            .iter()
            .filter(|track| {
                contains_ascii_case_insensitive(&track.title, &needle)
                    || contains_ascii_case_insensitive(&track.artist_name, &needle)
                    || contains_ascii_case_insensitive(&track.album_title, &needle)
            })
            .cloned()
            .collect();

        SearchResults {
            query: query.to_string(),
            artists,
            albums,
            tracks,
        }
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
    pub extension: String,
    pub file_size_bytes: Option<u64>,
    pub modified_at_unix_seconds: Option<i64>,
    pub content_hash: Option<String>,
    pub relative_path: String,
    pub stream_url: String,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
        let embedded_metadata =
            read_embedded_metadata(&path, &mut scan_errors, observed_at_unix_seconds);
        let metadata = canonical_metadata(embedded_metadata.as_ref(), &folder_metadata);
        let observed_metadata = metadata_observations(
            embedded_metadata,
            &folder_metadata,
            observed_at_unix_seconds,
        );
        let artist_id = stable_id("artist", &metadata.artist_name.to_ascii_lowercase());
        let album_key = format!(
            "{}::{}",
            metadata.artist_name.to_ascii_lowercase(),
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
                artist_id: artist_id.clone(),
                artist_name: metadata.artist_name.clone(),
                year: metadata.year,
                track_count: 0,
                artwork_path: find_album_artwork(path.parent().unwrap_or(&root)),
            })
            .track_count += 1;

        let artist = artist_builders
            .entry(artist_id.clone())
            .or_insert_with(|| ArtistBuilder {
                id: artist_id.clone(),
                name: metadata.artist_name.clone(),
                album_ids: Vec::new(),
                track_count: 0,
            });
        artist.track_count += 1;
        if !artist.album_ids.contains(&album_id) {
            artist.album_ids.push(album_id.clone());
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
            extension,
            file_size_bytes: file.file_size_bytes,
            modified_at_unix_seconds: file.modified_at_unix_seconds,
            content_hash: file.content_hash,
            relative_path,
            stream_url: format!("/api/tracks/{}/stream", track_id),
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
                .map(|_| format!("/api/albums/{}/artwork", album.id));
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
    album_title: String,
    year: Option<u16>,
    track_number: Option<u16>,
}

impl TrackMetadataObservation {
    fn folder_path(metadata: &TrackMetadata, observed_at_unix_seconds: i64) -> Self {
        Self {
            source: "folder_path".to_string(),
            confidence: 0.55,
            observed_at_unix_seconds,
            approval_state: MetadataApprovalState::Observed,
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
    }

    fn embedded_tag(tag: &lofty::tag::Tag, observed_at_unix_seconds: i64) -> Option<Self> {
        let album_artist_name = clean_tag_value(tag, ItemKey::AlbumArtist)
            .or_else(|| first_clean_tag_value(tag, ItemKey::AlbumArtists));

        let observation = Self {
            source: "embedded_tag".to_string(),
            confidence: 0.95,
            observed_at_unix_seconds,
            approval_state: MetadataApprovalState::Observed,
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
            lyrics: clean_tag_value(tag, ItemKey::Lyrics)
                .or_else(|| clean_tag_value(tag, ItemKey::UnsyncLyrics)),
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

        observation.has_metadata().then_some(observation)
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
            let content_hash = match hash_file_contents(&path) {
                Ok(hash) => Some(hash),
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

fn hash_file_contents(path: &Path) -> Result<String, std::io::Error> {
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

    Ok(hex_lower(&hasher.finalize()))
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

fn read_embedded_metadata(
    path: &Path,
    scan_errors: &mut Vec<ScanIssue>,
    observed_at_unix_seconds: i64,
) -> Option<TrackMetadataObservation> {
    let tagged_file = match Probe::open(path)
        .map(|probe| probe.options(ParseOptions::new().read_properties(false)))
        .and_then(Probe::read)
    {
        Ok(tagged_file) => tagged_file,
        Err(error) => {
            scan_errors.push(ScanIssue {
                path: path.display().to_string(),
                message: format!("metadata read failed: {error}"),
            });
            return None;
        }
    };

    tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .and_then(|tag| TrackMetadataObservation::embedded_tag(tag, observed_at_unix_seconds))
}

fn metadata_observations(
    embedded_metadata: Option<TrackMetadataObservation>,
    folder_metadata: &TrackMetadata,
    observed_at_unix_seconds: i64,
) -> Vec<TrackMetadataObservation> {
    let mut observations = Vec::new();

    if let Some(embedded_metadata) = embedded_metadata {
        observations.push(embedded_metadata);
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
        album_title: embedded_metadata
            .and_then(|metadata| metadata.album_title.clone())
            .unwrap_or_else(|| folder_metadata.album_title.clone()),
        year: embedded_metadata
            .and_then(|metadata| metadata.year)
            .or(folder_metadata.year),
        track_number: embedded_metadata
            .and_then(|metadata| metadata.track_number)
            .or(folder_metadata.track_number),
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
        album_title,
        year,
        track_number,
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

fn u32_to_u16(value: u32) -> Option<u16> {
    u16::try_from(value).ok()
}

fn find_album_artwork(album_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(album_dir).ok()?;
    let mut images = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && has_extension(&path, IMAGE_EXTENSIONS) {
            images.push(path);
        }
    }

    images.sort_by_key(|path| {
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
    });

    images.into_iter().next()
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

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value.to_ascii_lowercase().contains(needle)
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
    fn search_matches_tracks_albums_and_artists() {
        let fixture = TestFixture::new("search");
        fixture.write("1994 - Paramparcad/Darkwood Dub - Brzi Vavilon.mp3");
        fixture.write("1996 - U Nedogled/Darkwood Dub - U Nedogled.mp3");
        let library = scan_local_library(&fixture.root).expect("scan fixture");
        let results = library.search("vavilon");

        assert!(!results.tracks.is_empty());
        assert!(
            results
                .tracks
                .iter()
                .any(|track| track.title.contains("Vavilon"))
        );
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
    fn canonical_metadata_prefers_embedded_tags_with_folder_fallbacks() {
        let folder = TrackMetadata {
            title: "Folder Title".to_string(),
            artist_name: "Folder Artist".to_string(),
            album_title: "Folder Album".to_string(),
            year: Some(1994),
            track_number: Some(7),
        };
        let embedded = TrackMetadataObservation {
            source: "embedded_tag".to_string(),
            confidence: 0.95,
            observed_at_unix_seconds: 1_800_000_000,
            approval_state: MetadataApprovalState::Observed,
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
