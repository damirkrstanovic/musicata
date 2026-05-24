//! Core domain and provider interfaces for Musicata.
//!
//! The initial implementation ships a local-disk provider, but the domain model
//! deliberately describes music independently from the source that provided it.

use serde::Serialize;
use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display},
    fs,
    hash::{Hash, Hasher},
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

    for file in files {
        let path = file.path;
        let metadata = infer_track_metadata(&root, &path);
        let artist_id = stable_id("artist", &metadata.artist_name.to_ascii_lowercase());
        let album_key = format!(
            "{}::{}",
            metadata.artist_name.to_ascii_lowercase(),
            metadata.album_title.to_ascii_lowercase()
        );
        let album_id = stable_id("album", &album_key);
        let relative_path = relative_display_path(&root, &path);
        let track_id = stable_id("track", &relative_path);
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

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
            relative_path,
            stream_url: format!("/api/tracks/{track_id}/stream"),
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
}

#[derive(Clone, Debug)]
struct TrackMetadata {
    title: String,
    artist_name: String,
    album_title: String,
    year: Option<u16>,
    track_number: Option<u16>,
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
                    });
                    continue;
                }
            };
            files.push(DiscoveredAudioFile {
                path,
                file_size_bytes: Some(file_metadata.len()),
                modified_at_unix_seconds: file_metadata
                    .modified()
                    .ok()
                    .and_then(system_time_to_unix_seconds),
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
        assert!(library.scan_errors.is_empty());
        assert!(
            library
                .tracks
                .iter()
                .all(|track| track.file_size_bytes == Some(0))
        );
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
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");
            fs::write(path, []).expect("write fixture file");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
