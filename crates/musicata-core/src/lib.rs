//! Core domain and provider interfaces for Musicata.
//!
//! The initial implementation ships a local-disk provider, but the domain model
//! deliberately describes music independently from the source that provided it.

use lofty::{
    config::ParseOptions, file::AudioFile, file::FileType, file::TaggedFileExt,
    picture::PictureType, prelude::Accessor, probe::Probe, tag::ItemKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display},
    fs,
    hash::{Hash, Hasher},
    io::{Read, Seek},
    path::{Path, PathBuf},
};

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "opus", "wav"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const LYRIC_SIDECARS: &[(&str, &str, f32)] =
    &[("lrc", "sidecar_lrc", 0.9), ("txt", "sidecar_txt", 0.8)];
// Full-file hashing is too expensive for first imports of mounted multi-GB libraries.
// Keep synchronous hashes for tiny files and move deep fingerprinting to a later worker.
const INLINE_CONTENT_HASH_LIMIT_BYTES: u64 = 1024 * 1024;

/// What a music source is able to do. Sources are deliberately uneven: a local
/// folder or an SMB share can be scanned and searched, while an internet-radio
/// source can only stream. Each provider advertises its capabilities so the API
/// and UI can hide affordances a source does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderCapabilities {
    /// Can enumerate its whole catalogue into the merged library (`scan`).
    pub can_scan: bool,
    /// Can be navigated hierarchically without a full scan.
    pub can_browse: bool,
    /// Can answer text searches against its own catalogue.
    pub can_search: bool,
    /// Can produce a playable audio stream for its items.
    pub can_stream: bool,
}

impl ProviderCapabilities {
    /// On-disk sources (local folder, SMB share): full library semantics.
    pub const DISK: Self = Self {
        can_scan: true,
        can_browse: true,
        can_search: true,
        can_stream: true,
    };
    /// Streaming sources with no scannable catalogue, but a flat list you can
    /// browse (internet radio: no full-library scan, but its stations are listed
    /// via [`browse`](MusicProvider) and played via [`resolve`](MusicProvider)).
    pub const STREAM_ONLY: Self = Self {
        can_scan: false,
        can_browse: true,
        can_search: false,
        can_stream: true,
    };
}

/// One entry returned by a provider's `browse` — a station, folder, or item in a
/// non-scannable source's hierarchy. Scannable sources merge into the library and
/// are browsed through the library endpoints instead; this is for sources like
/// internet radio that are navigated directly.
#[derive(Clone, Debug, Serialize)]
pub struct BrowseEntry {
    /// Stable id of this entry within its provider (e.g. a radio station id).
    pub id: String,
    pub title: String,
    /// Optional secondary line (e.g. a station's homepage host or a genre).
    pub subtitle: Option<String>,
    /// A directly-playable stream URL when known at browse time (radio stations
    /// carry theirs); `None` when the URL must be obtained via `resolve`.
    pub stream_url: Option<String>,
    pub homepage_url: Option<String>,
    /// Whether this entry is a navigable container (a folder) rather than a leaf.
    pub is_container: bool,
}

/// How to play one resolved item — the output of a provider's `resolve`, mirroring
/// Music Assistant's stream-details/stream split. For internet radio it is just the
/// station's stream URL; future providers may compute a short-lived URL on demand.
#[derive(Clone, Debug, Serialize)]
pub struct StreamSpec {
    pub url: String,
    /// A display title for the stream (station name); shown while it plays.
    pub title: Option<String>,
    /// MIME type when the provider knows it; clients may sniff otherwise.
    pub content_type: Option<String>,
}

pub trait MusicProvider {
    fn provider_id(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn scan(&self) -> Result<Library, ScanError>;
}

/// A `Read + Seek` source — what lofty needs to parse tags. Local files and the
/// SMB read adapter both satisfy it.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// One entry returned by [`SourceFs::read_dir`].
#[derive(Clone, Debug)]
pub struct FsEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_file: bool,
    pub size: Option<u64>,
    pub modified_at_unix_seconds: Option<i64>,
}

/// Filesystem abstraction the scanner walks. A `LocalFs` reads the local disk; an
/// SMB-backed implementation in the server reads a network share over the wire.
/// Paths are logical paths under [`SourceFs::root`] (so the existing path-based
/// metadata inference keeps working unchanged for every backend).
pub trait SourceFs {
    /// List the immediate children of `dir`.
    fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<FsEntry>>;
    /// Open `path` for tag parsing / hashing.
    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ReadSeek + '_>>;
    /// Read a small text file whole (lyric sidecars).
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;
    /// Stat a single path.
    fn stat(&self, path: &Path) -> std::io::Result<FsEntry>;
    /// The logical root of this source, used for relative display paths.
    fn root(&self) -> &Path;
    /// Whether `path` exists. Defaults to a `stat` probe.
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
}

/// The local-disk [`SourceFs`]: a thin wrapper over `std::fs` rooted at a folder.
#[derive(Clone, Debug)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SourceFs for LocalFs {
    fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<FsEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            // Resolve size/mtime best-effort; a metadata failure leaves them unset
            // rather than dropping the entry.
            let metadata = entry.metadata().ok();
            entries.push(FsEntry {
                path,
                is_dir: file_type.is_dir(),
                is_file: file_type.is_file(),
                size: metadata.as_ref().map(|m| m.len()),
                modified_at_unix_seconds: metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(system_time_to_unix_seconds),
            });
        }
        Ok(entries)
    }

    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ReadSeek + '_>> {
        Ok(Box::new(fs::File::open(path)?))
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        fs::read_to_string(path)
    }

    fn stat(&self, path: &Path) -> std::io::Result<FsEntry> {
        let metadata = fs::metadata(path)?;
        Ok(FsEntry {
            path: path.to_path_buf(),
            is_dir: metadata.is_dir(),
            is_file: metadata.is_file(),
            size: Some(metadata.len()),
            modified_at_unix_seconds: metadata
                .modified()
                .ok()
                .and_then(system_time_to_unix_seconds),
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }
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

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::DISK
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
    /// Served URL for an externally-acquired artist image, or `None` (the UI shows a
    /// monogram). Set by the artwork fill pass, not by the scanner.
    #[serde(default)]
    pub artwork_url: Option<String>,
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

/// An internet radio station — a named external stream. The first non-library music
/// source (see docs/plugins.md).
#[derive(Clone, Debug, Serialize)]
pub struct RadioStation {
    pub id: String,
    pub name: String,
    pub stream_url: String,
    pub homepage_url: Option<String>,
    pub created_at_unix_seconds: i64,
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
    Seek {
        position_seconds: f64,
    },
    SetVolume {
        volume: u8,
    },
    SetRepeat {
        mode: RepeatMode,
    },
    SetShuffle {
        enabled: bool,
    },
    Clear,
    PlayTracks {
        track_ids: Vec<String>,
        /// Which queue position to start playing from (default 0). Lets a controller
        /// set the whole queue and start at the clicked track in a single command —
        /// avoiding a `play_tracks` + `play_queue_index` two-step that would briefly
        /// broadcast the wrong now-playing track and restart browser audio.
        #[serde(default)]
        start_index: usize,
    },
    Enqueue {
        track_ids: Vec<String>,
    },
    PlayQueueIndex {
        index: usize,
    },
    RemoveQueueItem {
        index: usize,
    },
    MoveQueueItem {
        from: usize,
        to: usize,
    },
    /// Play an external stream directly (e.g. an internet radio station). Unlike
    /// `PlayTracks`, the URL is not a library track and needs no resolution.
    PlayStream {
        url: String,
        title: String,
    },
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

/// Progress emitted during a scan so callers can show how far along it is.
#[derive(Clone, Debug)]
pub enum ScanProgress {
    /// Walking the source tree; `found` audio files discovered so far.
    Discovering { found: usize },
    /// Discovery finished; `files` audio files were found.
    Discovered { files: usize },
    /// Reading tags: `done` of `total` files processed. `detail` carries extra live
    /// info for concurrent scans (throughput, ETA, parallelism).
    Processing {
        done: usize,
        total: usize,
        detail: Option<String>,
    },
}

pub fn scan_local_library(root: &Path) -> Result<Library, ScanError> {
    scan_local_library_incremental(root, None, &mut |_| {})
}

/// Scan the local library, reusing unchanged files' metadata from `prior` (an
/// earlier scan / the stored library) so only new or modified files are re-read.
pub fn scan_local_library_incremental(
    root: &Path,
    prior: Option<&Library>,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<Library, ScanError> {
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

    scan_source_incremental(&LocalFs::new(&root), "local-disk", prior, progress)
}

/// Walk any [`SourceFs`] and build a [`Library`] attributed to `provider_id`.
/// This is the shared scanner: the local-disk and SMB providers both feed it,
/// only differing in how the underlying filesystem is read.
pub fn scan_source(fs: &dyn SourceFs, provider_id: &str) -> Result<Library, ScanError> {
    scan_source_incremental(fs, provider_id, None, &mut |_| {})
}

/// Incremental scan: the directory walk (cheap — one listing per folder, with each
/// entry's size + mtime) finds every file, but tags are only read for files that are
/// **new or whose size/mtime changed** since `prior`. Unchanged files reuse their
/// already-parsed track (metadata, duration, hash) wholesale — the expensive part,
/// especially over a network share. Mirrors how Jellyfin skips metadata re-reads via
/// DateModified comparison. Reports [`ScanProgress`] as it goes.
pub fn scan_source_incremental(
    fs: &dyn SourceFs,
    provider_id: &str,
    prior: Option<&Library>,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<Library, ScanError> {
    let root = fs.root().to_path_buf();
    let (files, walk_errors) = discover_audio_files(fs, &root, progress)?;
    let index = PriorIndex::from_library(prior, provider_id);

    let total = files.len();
    progress(ScanProgress::Discovered { files: total });
    let step = (total / 100).max(1); // ~100 updates max for the sequential path
    let observed_at_unix_seconds = current_unix_seconds();

    // Sequential build (local disk). The SMB provider runs `build_track` across files
    // concurrently instead; both feed `assemble_library`.
    let mut built = Vec::with_capacity(total);
    for (position, file) in files.into_iter().enumerate() {
        if (position + 1) % step == 0 || position + 1 == total {
            progress(ScanProgress::Processing {
                done: position + 1,
                total,
                detail: None,
            });
        }
        built.push(build_track(
            fs,
            &root,
            provider_id,
            &file,
            &index,
            observed_at_unix_seconds,
        ));
    }

    Ok(assemble_library(provider_id, &root, built, walk_errors))
}

/// Walk a source and return its audio files (sorted by path) plus any walk errors.
/// One directory listing per folder; the entries carry size + mtime for free.
pub fn discover_audio_files(
    fs: &dyn SourceFs,
    root: &Path,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<(Vec<DiscoveredAudioFile>, Vec<ScanIssue>), ScanError> {
    let mut files = Vec::new();
    let mut scan_errors = Vec::new();
    progress(ScanProgress::Discovering { found: 0 });
    collect_audio_files(fs, root, &mut files, &mut scan_errors, true, progress)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((files, scan_errors))
}

/// A prior scan indexed for incremental reuse: tracks by their source-relative path,
/// and albums' carried-forward artist + cover art. Owned (not borrowed) so it can be
/// shared across concurrent scan tasks.
pub struct PriorIndex {
    tracks: std::collections::HashMap<String, Track>,
    /// album id → (album-artist id, album-artist name, cover-art path)
    albums: std::collections::HashMap<String, (String, String, Option<PathBuf>)>,
}

impl PriorIndex {
    pub fn from_library(prior: Option<&Library>, provider_id: &str) -> Self {
        let mut tracks = std::collections::HashMap::new();
        let mut albums = std::collections::HashMap::new();
        if let Some(library) = prior {
            for track in &library.tracks {
                if track.provider.provider_id == provider_id {
                    tracks.insert(track.relative_path.clone(), track.clone());
                }
            }
            for album in &library.albums {
                albums.insert(
                    album.id.clone(),
                    (
                        album.artist_id.clone(),
                        album.artist_name.clone(),
                        album.artwork_path.clone(),
                    ),
                );
            }
        }
        Self { tracks, albums }
    }
}

/// One built track plus the data `assemble_library` needs to aggregate it. For a
/// reused (unchanged) file `identity` is `None` and the track keeps its prior id; for
/// a freshly-read file `identity` holds the content/metadata identity and the final
/// id is assigned (deduped) during assembly.
pub struct BuiltTrack {
    pub track: Track,
    pub identity: Option<String>,
    pub album_artist_id: String,
    pub album_artist_name: String,
    pub artwork_path: Option<PathBuf>,
    pub issues: Vec<ScanIssue>,
    /// Whether reading this file hit a (network/IO) read error — distinct from a tag
    /// parse failure. Used by the concurrent scanner's adaptive limiter.
    pub io_error: bool,
}

/// Build one track from a discovered file: reuse the prior parse if size + mtime are
/// unchanged, otherwise read its tags. Pure given `fs` + `prior`, so it can run
/// concurrently across files (the SMB provider does exactly that).
pub fn build_track(
    fs: &dyn SourceFs,
    root: &Path,
    provider_id: &str,
    file: &DiscoveredAudioFile,
    prior: &PriorIndex,
    observed_at_unix_seconds: i64,
) -> BuiltTrack {
    let path = file.path.clone();
    let relative_path = relative_display_path(root, &path);

    // Fast path: unchanged file → reuse its parsed track wholesale (no tag read).
    if let Some(previous) = prior.tracks.get(&relative_path)
        && file.file_size_bytes.is_some()
        && file.file_size_bytes == previous.file_size_bytes
        && file.modified_at_unix_seconds == previous.modified_at_unix_seconds
    {
        let mut track = previous.clone();
        track.path = path;
        let (album_artist_id, album_artist_name) = prior
            .albums
            .get(&track.album_id)
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .unwrap_or_else(|| (track.artist_id.clone(), track.artist_name.clone()));
        let artwork_path = prior
            .albums
            .get(&track.album_id)
            .and_then(|(_, _, artwork)| artwork.clone());
        return BuiltTrack {
            track,
            identity: None,
            album_artist_id,
            album_artist_name,
            artwork_path,
            issues: Vec::new(),
            io_error: false,
        };
    }

    // Full path: a new or modified file — read its tags.
    let mut issues = Vec::new();
    let folder_metadata = infer_track_metadata(root, &path);
    let (embedded_metadata, duration_seconds) =
        read_embedded_metadata(fs, &path, &mut issues, observed_at_unix_seconds);
    let sidecar_lyrics = read_sidecar_lyrics(fs, &path, &mut issues, observed_at_unix_seconds);
    // A read that produced neither metadata nor a parsed file is most likely an IO
    // failure (the tag read errored), not just a tagless file — signal it for backoff.
    let io_error = embedded_metadata.is_none() && !issues.is_empty();
    let metadata = canonical_metadata(embedded_metadata.as_ref(), &folder_metadata);
    let observed_metadata = metadata_observations(
        embedded_metadata,
        sidecar_lyrics,
        &folder_metadata,
        observed_at_unix_seconds,
    );
    let album_artist_name = metadata
        .album_artist_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| metadata.artist_name.clone());
    // Stage A: name-based identity (mbid wired in Stage B).
    let (artist_id, album_artist_id, album_id) = derive_ids(
        &metadata.artist_name,
        &album_artist_name,
        &metadata.album_title,
        None,
        None,
    );
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let identity = build_track_identity(
        &metadata,
        &extension,
        file.file_size_bytes,
        file.modified_at_unix_seconds,
        file.content_hash.as_deref(),
    );
    // Reuse the prior album's cover art if known, else discover a folder cover; failing
    // that, fall back to this track's *embedded* artwork by pointing `artwork_path` at
    // the audio file itself (the server extracts the picture on demand). So an album with
    // no `cover.jpg` but tagged-in art still shows a cover instead of the monogram.
    let has_embedded_artwork = observed_metadata
        .iter()
        .any(|observation| observation.embedded_artwork_count > 0);
    let artwork_path = prior
        .albums
        .get(&album_id)
        .and_then(|(_, _, artwork)| artwork.clone())
        .or_else(|| find_album_artwork(fs, path.parent().unwrap_or(root)))
        .or_else(|| has_embedded_artwork.then(|| path.clone()));

    let track = Track {
        id: String::new(), // assigned (deduped) in assemble_library
        provider: ProviderMapping {
            provider_id: provider_id.to_string(),
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
        content_hash: file.content_hash.clone(),
        relative_path,
        stream_url: String::new(), // assigned in assemble_library
        added_at_unix_seconds: None,
        path,
    };

    BuiltTrack {
        track,
        identity: Some(identity),
        album_artist_id,
        album_artist_name,
        artwork_path,
        issues,
        io_error,
    }
}

/// Fold built tracks into a finished [`Library`]: assign final (deduped) ids to
/// freshly-read tracks in a deterministic path order, aggregate albums/artists, and
/// sort. Sequential and cheap; shared by the sequential and concurrent scanners.
pub fn assemble_library(
    provider_id: &str,
    root: &Path,
    mut built: Vec<BuiltTrack>,
    mut scan_errors: Vec<ScanIssue>,
) -> Library {
    // Deterministic id assignment: order by source-relative path (matches the
    // discovery sort), so re-scans assign the same disambiguated ids.
    built.sort_by(|left, right| left.track.relative_path.cmp(&right.track.relative_path));

    let mut tracks = Vec::with_capacity(built.len());
    let mut album_builders: BTreeMap<String, AlbumBuilder> = BTreeMap::new();
    let mut artist_builders: BTreeMap<String, ArtistBuilder> = BTreeMap::new();
    let mut track_id_counts = BTreeMap::new();

    for mut item in built {
        scan_errors.append(&mut item.issues);
        if let Some(identity) = item.identity {
            let id = unique_track_id(&identity, &mut track_id_counts);
            item.track.stream_url = format!("/api/tracks/{id}/stream");
            item.track.id = id;
        }
        aggregate_track(
            &item.track,
            &item.album_artist_id,
            &item.album_artist_name,
            item.artwork_path,
            &mut album_builders,
            &mut artist_builders,
        );
        tracks.push(item.track);
    }

    finalize_library(
        provider_id.to_string(),
        root.display().to_string(),
        tracks,
        album_builders,
        artist_builders,
        scan_errors,
    )
}

/// Build the sorted artist/album entities from the aggregation builders and assemble
/// the final library. Shared by `assemble_library` (the scanner) and
/// [`regroup_library_with_overrides`] (metadata enrichment) so both produce identical
/// entity rows.
fn finalize_library(
    provider_id: String,
    source_root: String,
    mut tracks: Vec<Track>,
    album_builders: BTreeMap<String, AlbumBuilder>,
    artist_builders: BTreeMap<String, ArtistBuilder>,
    scan_errors: Vec<ScanIssue>,
) -> Library {
    let mut artists: Vec<_> = artist_builders
        .into_values()
        .map(|artist| Artist {
            id: artist.id,
            name: artist.name,
            album_count: artist.album_ids.len(),
            track_count: artist.track_count,
            artwork_url: None,
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

    Library {
        provider_id,
        source_root,
        artists,
        albums,
        tracks,
        scan_errors,
    }
}

/// Per-track canonical overrides from an external metadata source (MusicBrainz
/// enrichment). Only `Some` fields are applied; the rest keep the track's current
/// canonical value. The caller decides which fields to set (e.g. skipping fields the
/// file already carries as embedded tags).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackCanonicalOverride {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    /// The album artist (regroups the album). When unset, the track's existing album
    /// grouping's artist is kept.
    pub album_artist_name: Option<String>,
    pub year: Option<u16>,
    pub track_number: Option<u16>,
    pub disc_number: Option<u16>,
}

impl TrackCanonicalOverride {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Apply per-track canonical overrides (keyed by track id) and **re-derive** the
/// artist/album entities and each track's `artist_id`/`album_id`. Reuses the scanner's
/// id derivation and aggregation (`aggregate_track` + [`finalize_library`]), so the
/// result is identical to a fresh scan that had read those values — keeping the
/// denormalized track columns and the entity tables consistent after enrichment.
///
/// Album artwork is carried over from the pre-regroup albums (by their old `album_id`),
/// so regrouping doesn't drop covers. Track ids, stream urls, provenance, and
/// `added_at` are preserved untouched.
pub fn regroup_library_with_overrides(
    library: Library,
    overrides: &BTreeMap<String, TrackCanonicalOverride>,
) -> Library {
    // Carry over each old album's artwork + its album-artist (used for tracks whose
    // album-artist isn't being overridden), keyed by the pre-regroup album_id.
    let mut artwork_by_old_album: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut album_artist_by_old_album: BTreeMap<String, String> = BTreeMap::new();
    for album in &library.albums {
        if let Some(path) = &album.artwork_path {
            artwork_by_old_album.insert(album.id.clone(), path.clone());
        }
        album_artist_by_old_album.insert(album.id.clone(), album.artist_name.clone());
    }

    let Library {
        provider_id,
        source_root,
        mut tracks,
        scan_errors,
        ..
    } = library;

    let mut album_builders: BTreeMap<String, AlbumBuilder> = BTreeMap::new();
    let mut artist_builders: BTreeMap<String, ArtistBuilder> = BTreeMap::new();

    for track in &mut tracks {
        let old_album_id = track.album_id.clone();
        let override_for = overrides.get(&track.id);
        if let Some(over) = override_for {
            if let Some(title) = &over.title {
                track.title = title.clone();
            }
            if let Some(artist_name) = &over.artist_name {
                track.artist_name = artist_name.clone();
            }
            if let Some(album_title) = &over.album_title {
                track.album_title = album_title.clone();
            }
            if over.year.is_some() {
                track.year = over.year;
            }
            if over.track_number.is_some() {
                track.track_number = over.track_number;
            }
            if over.disc_number.is_some() {
                track.disc_number = over.disc_number;
            }
        }

        // Album artist: the override wins; else keep the old album's artist; else the
        // track artist (matching the scanner's fallback in `build_track`).
        let album_artist_name = override_for
            .and_then(|over| over.album_artist_name.clone())
            .or_else(|| album_artist_by_old_album.get(&old_album_id).cloned())
            .unwrap_or_else(|| track.artist_name.clone());

        // Recompute ids exactly as the scanner does (via the shared `derive_ids`).
        let (artist_id, album_artist_id, album_id) = derive_ids(
            &track.artist_name,
            &album_artist_name,
            &track.album_title,
            None,
            None,
        );
        track.artist_id = artist_id;
        track.album_id = album_id;

        let artwork_path = artwork_by_old_album.get(&old_album_id).cloned();
        aggregate_track(
            track,
            &album_artist_id,
            &album_artist_name,
            artwork_path,
            &mut album_builders,
            &mut artist_builders,
        );
    }

    finalize_library(
        provider_id,
        source_root,
        tracks,
        album_builders,
        artist_builders,
        scan_errors,
    )
}

/// Fold one track into the album/artist aggregates: count it against its own
/// artist, register/seed its album under the album-artist, and link the album to
/// that album-artist. Shared by the read and reuse paths so both aggregate
/// identically.
fn aggregate_track(
    track: &Track,
    album_artist_id: &str,
    album_artist_name: &str,
    artwork_path: Option<PathBuf>,
    album_builders: &mut BTreeMap<String, AlbumBuilder>,
    artist_builders: &mut BTreeMap<String, ArtistBuilder>,
) {
    let album = album_builders
        .entry(track.album_id.clone())
        .or_insert_with(|| AlbumBuilder {
            id: track.album_id.clone(),
            title: track.album_title.clone(),
            artist_id: album_artist_id.to_string(),
            artist_name: album_artist_name.to_string(),
            year: track.year,
            track_count: 0,
            artwork_path: None,
        });
    album.track_count += 1;
    // The album takes the first cover any of its tracks supplies (a folder image or an
    // embedded fallback), so it isn't left coverless just because its first track lacked
    // one.
    if album.artwork_path.is_none() {
        album.artwork_path = artwork_path;
    }

    // Count the track against its own (track) artist.
    artist_builders
        .entry(track.artist_id.clone())
        .or_insert_with(|| ArtistBuilder {
            id: track.artist_id.clone(),
            name: track.artist_name.clone(),
            album_ids: Vec::new(),
            track_count: 0,
        })
        .track_count += 1;

    // Associate the album with its album-artist (the same entry for non-compilations).
    let owning_artist = artist_builders
        .entry(album_artist_id.to_string())
        .or_insert_with(|| ArtistBuilder {
            id: album_artist_id.to_string(),
            name: album_artist_name.to_string(),
            album_ids: Vec::new(),
            track_count: 0,
        });
    if !owning_artist.album_ids.contains(&track.album_id) {
        owning_artist.album_ids.push(track.album_id.clone());
    }
}

/// Combine the libraries of several sources into one merged library. Tracks are
/// concatenated (their ids are globally unique and each keeps its own
/// `provider_id`, so per-source attribution survives). Album and artist rows are
/// unioned by id — disjoint sources keep their aggregates exactly, and the rare
/// same-id collision (a duplicate album across two sources) sums its counts.
pub fn merge_libraries(libraries: Vec<Library>) -> Library {
    if libraries.len() == 1 {
        return libraries.into_iter().next().expect("len checked");
    }

    let mut tracks = Vec::new();
    let mut artists: BTreeMap<String, Artist> = BTreeMap::new();
    let mut albums: BTreeMap<String, Album> = BTreeMap::new();
    let mut scan_errors = Vec::new();
    let mut roots = Vec::new();
    // Track ids are unique within a source, but the same file present in two sources
    // (e.g. a local copy and a NAS copy) yields the same id — keep both as distinct
    // library entries by disambiguating the collision.
    let mut seen_track_ids: BTreeSet<String> = BTreeSet::new();

    for library in libraries {
        roots.push(library.source_root);
        scan_errors.extend(library.scan_errors);
        for artist in library.artists {
            artists
                .entry(artist.id.clone())
                .and_modify(|existing| {
                    existing.album_count += artist.album_count;
                    existing.track_count += artist.track_count;
                })
                .or_insert(artist);
        }
        for album in library.albums {
            albums
                .entry(album.id.clone())
                .and_modify(|existing| existing.track_count += album.track_count)
                .or_insert(album);
        }
        for mut track in library.tracks {
            if !seen_track_ids.insert(track.id.clone()) {
                let base = track.id.clone();
                let mut suffix = 2;
                let mut candidate = format!("{base}-{suffix}");
                while !seen_track_ids.insert(candidate.clone()) {
                    suffix += 1;
                    candidate = format!("{base}-{suffix}");
                }
                track.stream_url = format!("/api/tracks/{candidate}/stream");
                track.id = candidate;
            }
            tracks.push(track);
        }
    }

    tracks.sort_by(|left, right| {
        left.artist_name
            .cmp(&right.artist_name)
            .then_with(|| left.year.cmp(&right.year))
            .then_with(|| left.album_title.cmp(&right.album_title))
            .then_with(|| left.disc_number.cmp(&right.disc_number))
            .then_with(|| left.track_number.cmp(&right.track_number))
            .then_with(|| left.title.cmp(&right.title))
    });

    let mut artists: Vec<_> = artists.into_values().collect();
    artists.sort_by(|left, right| left.name.cmp(&right.name));

    let mut albums: Vec<_> = albums.into_values().collect();
    albums.sort_by(|left, right| {
        left.artist_name
            .cmp(&right.artist_name)
            .then_with(|| left.year.cmp(&right.year))
            .then_with(|| left.title.cmp(&right.title))
    });

    Library {
        provider_id: "musicata-merged".to_string(),
        source_root: roots.join(", "),
        artists,
        albums,
        tracks,
        scan_errors,
    }
}

/// One audio file found during discovery, with the cheap stat data (size + mtime)
/// the incremental scanner uses to decide reuse vs re-read.
#[derive(Clone, Debug)]
pub struct DiscoveredAudioFile {
    pub path: PathBuf,
    pub file_size_bytes: Option<u64>,
    pub modified_at_unix_seconds: Option<i64>,
    pub content_hash: Option<String>,
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

/// The audio files and subdirectories found in one directory (non-recursive).
#[derive(Default)]
pub struct DirListing {
    pub files: Vec<DiscoveredAudioFile>,
    pub subdirs: Vec<PathBuf>,
    pub issues: Vec<ScanIssue>,
}

/// List one directory: classify its entries into audio files (hashing small ones)
/// and subdirectories. A read error becomes an issue rather than failing. Used by
/// both the sequential walk and the SMB parallel walk (so each does one round-trip
/// per directory and shares the classification logic).
pub fn list_dir_audio(fs: &dyn SourceFs, dir: &Path) -> DirListing {
    let mut listing = DirListing::default();
    let entries = match fs.read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            listing.issues.push(ScanIssue {
                path: dir.display().to_string(),
                message: source.to_string(),
            });
            return listing;
        }
    };
    for entry in entries {
        if entry.is_dir {
            listing.subdirs.push(entry.path);
        } else if entry.is_file && has_extension(&entry.path, AUDIO_EXTENSIONS) {
            let content_hash = match hash_file_contents_if_small(fs, &entry.path, entry.size) {
                Ok(hash) => hash,
                Err(source) => {
                    listing.issues.push(ScanIssue {
                        path: entry.path.display().to_string(),
                        message: source.to_string(),
                    });
                    None
                }
            };
            listing.files.push(DiscoveredAudioFile {
                path: entry.path,
                file_size_bytes: entry.size,
                modified_at_unix_seconds: entry.modified_at_unix_seconds,
                content_hash,
            });
        }
    }
    listing
}

fn collect_audio_files(
    fs: &dyn SourceFs,
    dir: &Path,
    files: &mut Vec<DiscoveredAudioFile>,
    scan_errors: &mut Vec<ScanIssue>,
    required: bool,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<(), ScanError> {
    // A read failure on the required root is fatal; deeper failures are issues.
    if required && let Err(source) = fs.read_dir(dir) {
        return Err(ScanError::Io {
            path: dir.to_path_buf(),
            source,
        });
    }

    let mut listing = list_dir_audio(fs, dir);
    files.append(&mut listing.files);
    scan_errors.append(&mut listing.issues);
    // Periodic discovery progress so a slow network walk shows life.
    progress(ScanProgress::Discovering { found: files.len() });

    for subdir in listing.subdirs {
        collect_audio_files(fs, &subdir, files, scan_errors, false, progress)?;
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
    fs: &dyn SourceFs,
    path: &Path,
    file_size_bytes: Option<u64>,
) -> Result<Option<String>, std::io::Error> {
    // Only fingerprint files whose size is known and small; unknown or large
    // files fall back to size/mtime identity (see `build_track_identity`).
    match file_size_bytes {
        Some(size) if size <= INLINE_CONTENT_HASH_LIMIT_BYTES => {}
        _ => return Ok(None),
    }

    let mut file = fs.open(path)?;
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

/// Parse a tagged file from a [`SourceFs`] reader. Mirrors `Probe::open`'s
/// extension-driven file-type detection (falling back to content sniffing when the
/// extension is unknown), but works over any `Read + Seek` source — local or SMB.
fn probe_tagged_file(
    fs: &dyn SourceFs,
    path: &Path,
    read_properties: bool,
) -> Result<lofty::file::TaggedFile, Box<dyn Error>> {
    let reader = fs.open(path)?;
    let probe = Probe::new(reader).options(ParseOptions::new().read_properties(read_properties));
    let probe = match FileType::from_path(path) {
        Some(file_type) => probe.set_file_type(file_type),
        None => probe.guess_file_type()?,
    };
    Ok(probe.read()?)
}

/// Read the embedded tag observation and the track duration in one probe. Properties
/// are parsed so we get the stream length; a zero/unknown duration becomes `None`.
fn read_embedded_metadata(
    fs: &dyn SourceFs,
    path: &Path,
    scan_errors: &mut Vec<ScanIssue>,
    observed_at_unix_seconds: i64,
) -> (Option<TrackMetadataObservation>, Option<f64>) {
    // Prefer reading stream properties (so we get a duration), but some files — and
    // minimal/edge-case inputs — fail property parsing while their tags are still
    // readable. Fall back to a tags-only read in that case (duration stays `None`).
    let with_properties = probe_tagged_file(fs, path, true);
    let (tagged_file, duration) = match with_properties {
        Ok(tagged_file) => {
            let seconds = tagged_file.properties().duration().as_secs_f64();
            (tagged_file, (seconds > 0.0).then_some(seconds))
        }
        Err(_) => match probe_tagged_file(fs, path, false) {
            Ok(tagged_file) => (tagged_file, None),
            Err(error) => {
                scan_errors.push(ScanIssue {
                    path: path.display().to_string(),
                    message: format!("metadata read failed: {error}"),
                });
                return (None, None);
            }
        },
    };

    let observation = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .and_then(|tag| TrackMetadataObservation::embedded_tag(tag, observed_at_unix_seconds));
    (observation, duration)
}

fn read_sidecar_lyrics(
    fs: &dyn SourceFs,
    path: &Path,
    scan_errors: &mut Vec<ScanIssue>,
    observed_at_unix_seconds: i64,
) -> Option<TrackMetadataObservation> {
    for (extension, source, confidence) in LYRIC_SIDECARS {
        let sidecar_path = path.with_extension(extension);
        if !fs.exists(&sidecar_path) {
            continue;
        }

        match fs.read_to_string(&sidecar_path) {
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

/// Extract the cover picture embedded in an audio file's tags, returning the raw image
/// bytes (JPEG/PNG/… as stored). `audio_bytes` is the whole file — the picture lives in
/// its tag block — and `path` only supplies a file-type hint. Prefers the front-cover
/// picture, else the first one. `None` if the file has no embedded picture or can't be
/// parsed. The caller decides the image content type (e.g. by sniffing the bytes).
pub fn extract_embedded_cover(audio_bytes: &[u8], path: &Path) -> Option<Vec<u8>> {
    let cursor = std::io::Cursor::new(audio_bytes);
    let probe = Probe::new(cursor).options(ParseOptions::new().read_properties(false));
    let probe = match FileType::from_path(path) {
        Some(file_type) => probe.set_file_type(file_type),
        None => probe.guess_file_type().ok()?,
    };
    let tagged = probe.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pictures = tag.pictures();
    let picture = pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;
    Some(picture.data().to_vec())
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
    find_album_artwork_candidates_in(&LocalFs::new(album_dir), album_dir)
}

/// Artwork candidates within `album_dir`, read through a [`SourceFs`] so the same
/// discovery serves local folders and SMB shares.
fn find_album_artwork_candidates_in(fs: &dyn SourceFs, album_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs.read_dir(album_dir) else {
        return Vec::new();
    };
    let mut images = Vec::new();

    for entry in entries {
        if entry.is_file && has_extension(&entry.path, IMAGE_EXTENSIONS) {
            images.push(entry.path);
        }
    }

    images.sort_by(|left, right| {
        artwork_priority(left)
            .cmp(&artwork_priority(right))
            .then_with(|| left.cmp(right))
    });

    images
}

fn find_album_artwork(fs: &dyn SourceFs, album_dir: &Path) -> Option<PathBuf> {
    find_album_artwork_candidates_in(fs, album_dir)
        .into_iter()
        .next()
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

/// Fold a lowercase Latin letter carrying a diacritic to its base letter — strictly
/// combining-mark removal on existing scalars, never transliteration across scripts (so
/// "Сергей"/"史" are untouched and stay distinct). Covers the accented Latin letters that
/// actually appear in artist names; returns `None` for anything not folded. Input is
/// expected already-lowercased.
fn fold_diacritic(ch: char) -> Option<&'static str> {
    Some(match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ª' => "a",
        'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => "c",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'º' => "o",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'đ' | 'ð' => "d",
        'ł' => "l",
        'ś' | 'š' | 'ş' | 'ŝ' => "s",
        'ž' | 'ź' | 'ż' => "z",
        'ŕ' | 'ř' => "r",
        'ť' | 'ţ' => "t",
        'ğ' | 'ĝ' | 'ġ' | 'ģ' => "g",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        _ => return None,
    })
}

/// A normalized identity key for an artist name: fold diacritics, lowercase, drop a single
/// leading whole-word "the ", and collapse runs of non-alphanumerics to single spaces. So
/// "Beyoncé"≡"Beyonce", "The Beatles"≡"Beatles", "Motörhead"≡"Motorhead" — but genuinely
/// different names ("Fela Kuti" vs "Fela Anikulapo Kuti", "Therion" vs "The …") stay
/// distinct. Never returns empty (a name that normalizes away falls back to its raw,
/// lowercased form, so junk names don't all collapse into one artist).
pub fn normalize_artist_key(name: &str) -> String {
    let trimmed = name.trim();
    // Lowercase (Unicode-aware) then fold diacritics, char by char.
    let mut folded = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        for lower in ch.to_lowercase() {
            match fold_diacritic(lower) {
                Some(base) => folded.push_str(base),
                None => folded.push(lower),
            }
        }
    }
    // Strip at most one leading whole-word "the " (guards "Therion"/"Theatre…").
    let body = folded.strip_prefix("the ").unwrap_or(&folded);
    // Collapse non-alphanumerics to single spaces.
    let mut key = String::with_capacity(body.len());
    let mut last_space = true;
    for ch in body.chars() {
        if ch.is_alphanumeric() {
            key.push(ch);
            last_space = false;
        } else if !last_space {
            key.push(' ');
            last_space = true;
        }
    }
    let key = key.trim();
    if key.is_empty() {
        // Don't key on empty — fall back to the raw name so distinct junk names stay distinct.
        return trimmed.to_lowercase();
    }
    key.to_string()
}

/// The stable artist id: keyed on the MusicBrainz artist id when present (so variant
/// spellings that share one MBID merge), else on the normalized name. An mbid-keyed and a
/// name-keyed artist for the same person are distinct until both carry the MBID.
pub fn artist_identity(name: &str, mbid: Option<&str>) -> String {
    match mbid.map(str::trim).filter(|m| !m.is_empty()) {
        Some(mbid) => stable_id("artist", &format!("mbid:{}", mbid.to_lowercase())),
        None => stable_id("artist", &normalize_artist_key(name)),
    }
}

/// The stable album id: keyed on the (normalized or MBID-keyed) album-artist plus the
/// lowercased album title, mirroring [`artist_identity`] for the album-artist half.
pub fn album_identity(
    album_artist_name: &str,
    album_title: &str,
    album_artist_mbid: Option<&str>,
) -> String {
    let album_artist_key = match album_artist_mbid.map(str::trim).filter(|m| !m.is_empty()) {
        Some(mbid) => format!("mbid:{}", mbid.to_lowercase()),
        None => normalize_artist_key(album_artist_name),
    };
    let album_key = format!("{album_artist_key}::{}", album_title.to_ascii_lowercase());
    stable_id("album", &album_key)
}

/// Derive the (artist, album-artist, album) ids for a track from its names + optional
/// MusicBrainz artist ids. The single source of truth so the scanner (`build_track`) and
/// the re-grouper (`regroup_library_with_overrides`) can't drift.
fn derive_ids(
    artist_name: &str,
    album_artist_name: &str,
    album_title: &str,
    artist_mbid: Option<&str>,
    album_artist_mbid: Option<&str>,
) -> (String, String, String) {
    let artist_id = artist_identity(artist_name, artist_mbid);
    let album_artist_id = artist_identity(album_artist_name, album_artist_mbid);
    let album_id = album_identity(album_artist_name, album_title, album_artist_mbid);
    (artist_id, album_artist_id, album_id)
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
    use std::{cell::Cell, collections::BTreeSet, fs, io::Cursor, time::SystemTime};

    // Two names share an artist iff their normalized keys match.
    fn same_artist(a: &str, b: &str) -> bool {
        normalize_artist_key(a) == normalize_artist_key(b)
    }

    #[test]
    fn normalize_merges_safe_variants() {
        assert!(same_artist("Beyoncé", "Beyonce"));
        assert!(same_artist("Björk", "Bjork"));
        assert!(same_artist("Motörhead", "Motorhead"));
        assert!(same_artist("Sigur Rós", "Sigur Ros"));
        assert!(same_artist("The Beatles", "Beatles"));
        assert!(same_artist("  The   Beatles  ", "beatles"));
        assert!(same_artist("AC/DC", "AC DC"));
        assert!(same_artist("Guns N' Roses", "Guns N Roses"));
    }

    #[test]
    fn normalize_keeps_genuinely_different_names_apart() {
        assert!(!same_artist("Fela Kuti", "Fela Anikulapo Kuti"));
        assert!(!same_artist("The The", "The Beatles"));
        // Leading "the " is only stripped as a whole word — not inside a word.
        assert_eq!(normalize_artist_key("Therion"), "therion");
        assert_eq!(normalize_artist_key("Theatre of Tragedy"), "theatre of tragedy");
        assert!(!same_artist("Therion", "ion"));
        // No transliteration across scripts.
        assert!(!same_artist("Сергей", "Sergei"));
    }

    #[test]
    fn normalize_never_keys_on_empty() {
        // A name that normalizes away falls back to its raw form, so distinct junk names
        // (and "The ") don't all collapse into one artist.
        assert!(!normalize_artist_key("The ").is_empty());
        assert_ne!(normalize_artist_key("???"), normalize_artist_key("!!!"));
        assert_eq!(normalize_artist_key("The The"), "the");
    }

    #[test]
    fn artist_identity_prefers_mbid_then_name() {
        // Same MBID, different spellings → one artist.
        assert_eq!(
            artist_identity("Beatles", Some("mbid-x")),
            artist_identity("The Beatles", Some("mbid-x"))
        );
        // Different MBIDs, same name → distinct (genuinely different artists).
        assert_ne!(
            artist_identity("X", Some("a")),
            artist_identity("X", Some("b"))
        );
        // Name-keyed and mbid-keyed for the same name are distinct until both carry it.
        assert_ne!(artist_identity("X", None), artist_identity("X", Some("a")));
        // No mbid → falls back to the normalized name (so variants still merge).
        assert_eq!(
            artist_identity("Beyoncé", None),
            artist_identity("Beyonce", None)
        );
    }

    /// An in-memory [`SourceFs`] for exercising the scanner without a disk or a
    /// network share. Folder structure drives metadata (the fake byte content is
    /// not valid audio, so tag parsing fails and the scanner falls back to the
    /// path-derived metadata — exactly the path SMB scanning relies on too).
    struct FakeFs {
        root: PathBuf,
        files: Vec<(PathBuf, Vec<u8>)>,
        mtimes: BTreeMap<PathBuf, i64>,
        opens: Cell<usize>,
    }

    impl FakeFs {
        fn new(files: Vec<(PathBuf, Vec<u8>)>) -> Self {
            Self {
                root: PathBuf::from("/"),
                files,
                mtimes: BTreeMap::new(),
                opens: Cell::new(0),
            }
        }

        fn bump_mtime(&mut self, path: &str, mtime: i64) {
            self.mtimes.insert(PathBuf::from(path), mtime);
        }
    }

    impl SourceFs for FakeFs {
        fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<FsEntry>> {
            let mut seen_dirs = BTreeSet::new();
            let mut entries = Vec::new();
            for (path, bytes) in &self.files {
                let Ok(rest) = path.strip_prefix(dir) else {
                    continue;
                };
                let mut components = rest.components();
                let Some(first) = components.next() else {
                    continue;
                };
                let child = dir.join(first.as_os_str());
                if components.next().is_some() {
                    if seen_dirs.insert(child.clone()) {
                        entries.push(FsEntry {
                            path: child,
                            is_dir: true,
                            is_file: false,
                            size: None,
                            modified_at_unix_seconds: None,
                        });
                    }
                } else {
                    let modified = self.mtimes.get(&child).copied().unwrap_or(0);
                    entries.push(FsEntry {
                        path: child,
                        is_dir: false,
                        is_file: true,
                        size: Some(bytes.len() as u64),
                        modified_at_unix_seconds: Some(modified),
                    });
                }
            }
            Ok(entries)
        }

        fn open(&self, path: &Path) -> std::io::Result<Box<dyn ReadSeek + '_>> {
            self.opens.set(self.opens.get() + 1);
            let bytes = self
                .files
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(_, bytes)| bytes.clone())
                .ok_or(std::io::ErrorKind::NotFound)?;
            Ok(Box::new(Cursor::new(bytes)))
        }

        fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
            let bytes = self
                .files
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(_, bytes)| bytes.clone())
                .ok_or(std::io::ErrorKind::NotFound)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }

        fn stat(&self, path: &Path) -> std::io::Result<FsEntry> {
            if let Some((_, bytes)) = self.files.iter().find(|(c, _)| c == path) {
                return Ok(FsEntry {
                    path: path.to_path_buf(),
                    is_dir: false,
                    is_file: true,
                    size: Some(bytes.len() as u64),
                    modified_at_unix_seconds: Some(0),
                });
            }
            if self.files.iter().any(|(c, _)| c.starts_with(path)) {
                return Ok(FsEntry {
                    path: path.to_path_buf(),
                    is_dir: true,
                    is_file: false,
                    size: None,
                    modified_at_unix_seconds: None,
                });
            }
            Err(std::io::ErrorKind::NotFound.into())
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    #[test]
    fn scans_any_source_fs_and_merges_multiple_sources() {
        // The scanner derives the artist from the "Artist - Title" filename and the
        // album from the parent directory, so name the fakes accordingly.
        let source_a = FakeFs::new(vec![
            (
                PathBuf::from("/Album1/01 - ArtistA - One.mp3"),
                b"x".to_vec(),
            ),
            (
                PathBuf::from("/Album1/02 - ArtistA - Two.mp3"),
                b"y".to_vec(),
            ),
        ]);
        let source_b = FakeFs::new(vec![
            (
                PathBuf::from("/Album3/01 - ArtistA - Three.mp3"),
                b"z".to_vec(),
            ),
            (
                PathBuf::from("/Album2/01 - ArtistB - Four.mp3"),
                b"w".to_vec(),
            ),
        ]);

        let lib_a = scan_source(&source_a, "src-a").expect("scan a");
        let lib_b = scan_source(&source_b, "src-b").expect("scan b");
        assert_eq!(lib_a.tracks.len(), 2);
        assert!(
            lib_a
                .tracks
                .iter()
                .all(|t| t.provider.provider_id == "src-a")
        );

        let merged = merge_libraries(vec![lib_a, lib_b]);
        assert_eq!(merged.provider_id, "musicata-merged");
        assert_eq!(merged.tracks.len(), 4);

        // Per-track source attribution survives the merge.
        assert_eq!(
            merged
                .tracks
                .iter()
                .filter(|t| t.provider.provider_id == "src-a")
                .count(),
            2
        );
        assert_eq!(
            merged
                .tracks
                .iter()
                .filter(|t| t.provider.provider_id == "src-b")
                .count(),
            2
        );

        // ArtistA appears in both sources: counts sum, albums union (Album1 + Album3).
        let artist_a = merged
            .artists
            .iter()
            .find(|a| a.name == "ArtistA")
            .expect("ArtistA present");
        assert_eq!(artist_a.track_count, 3);
        assert_eq!(artist_a.album_count, 2);
        assert_eq!(merged.albums.len(), 3);
    }

    #[test]
    fn incremental_scan_reuses_unchanged_files_and_rereads_changed() {
        // Files >1MB so the walk doesn't hash them (hashing opens small files); this
        // isolates the tag-read, which is what incremental reuse must skip.
        let files = vec![
            (
                PathBuf::from("/Album1/01 - ArtistA - One.mp3"),
                vec![1u8; 1_200_000],
            ),
            (
                PathBuf::from("/Album1/02 - ArtistA - Two.mp3"),
                vec![2u8; 1_200_000],
            ),
        ];
        let fs = FakeFs::new(files.clone());

        // First scan: a full read — every file is opened (tags + sidecar probe).
        let first = scan_source(&fs, "src").expect("first scan");
        assert_eq!(first.tracks.len(), 2);
        let first_opens = fs.opens.get();
        assert!(first_opens > 0, "first scan must read files");

        // Second scan with the prior library and identical files: nothing is opened —
        // both tracks are reused, and the library is unchanged.
        let fs2 = FakeFs::new(files.clone());
        let second = scan_source_incremental(&fs2, "src", Some(&first), &mut |_| {})
            .expect("incremental scan");
        assert_eq!(fs2.opens.get(), 0, "unchanged files must not be re-read");
        assert_eq!(second.tracks.len(), 2);
        let mut a: Vec<_> = first.tracks.iter().map(|t| t.id.clone()).collect();
        let mut b: Vec<_> = second.tracks.iter().map(|t| t.id.clone()).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "reused tracks keep their identity");

        // Change one file's mtime: only that file is re-read, the other is reused.
        let mut fs3 = FakeFs::new(files);
        fs3.bump_mtime("/Album1/02 - ArtistA - Two.mp3", 999);
        let third =
            scan_source_incremental(&fs3, "src", Some(&first), &mut |_| {}).expect("changed scan");
        assert_eq!(third.tracks.len(), 2);
        // One file re-read (tags + sidecar) → opens are > 0 but far fewer than a full scan.
        assert!(fs3.opens.get() > 0);
        assert!(
            fs3.opens.get() < first_opens,
            "only the changed file is re-read"
        );
    }

    #[test]
    fn scanner_merges_normalized_artist_variants() {
        // Files >1MB so the walk doesn't hash; folder/filename metadata gives the artist.
        let files = vec![
            (PathBuf::from("/A/01 - Beyoncé - Hold Up.mp3"), vec![1u8; 1_200_000]),
            (PathBuf::from("/A/02 - Beyonce - Sorry.mp3"), vec![2u8; 1_200_000]),
            (PathBuf::from("/A/03 - The Beatles - Come Together.mp3"), vec![3u8; 1_200_000]),
            (PathBuf::from("/A/04 - Beatles - Something.mp3"), vec![4u8; 1_200_000]),
            (PathBuf::from("/A/05 - Fela Kuti - Zombie.mp3"), vec![5u8; 1_200_000]),
            (PathBuf::from("/A/06 - Fela Anikulapo Kuti - Water.mp3"), vec![6u8; 1_200_000]),
        ];
        let fs = FakeFs::new(files);
        let library = scan_source(&fs, "src").expect("scan");
        let id_for = |name: &str| -> String {
            library
                .tracks
                .iter()
                .find(|t| t.artist_name == name)
                .unwrap_or_else(|| panic!("no track for {name}"))
                .artist_id
                .clone()
        };
        // Safe variants merge; a genuinely different name stays separate.
        assert_eq!(id_for("Beyoncé"), id_for("Beyonce"));
        assert_eq!(id_for("The Beatles"), id_for("Beatles"));
        assert_ne!(id_for("Fela Kuti"), id_for("Fela Anikulapo Kuti"));
    }

    #[test]
    fn regroup_matches_scanner_ids() {
        let files = vec![
            (PathBuf::from("/A/01 - Beyoncé - Hold Up.mp3"), vec![1u8; 1_200_000]),
            (PathBuf::from("/A/02 - Beatles - Something.mp3"), vec![2u8; 1_200_000]),
        ];
        let fs = FakeFs::new(files);
        let library = scan_source(&fs, "src").expect("scan");
        let before: BTreeMap<String, (String, String)> = library
            .tracks
            .iter()
            .map(|t| (t.id.clone(), (t.artist_id.clone(), t.album_id.clone())))
            .collect();
        let regrouped = regroup_library_with_overrides(library, &BTreeMap::new());
        for track in &regrouped.tracks {
            assert_eq!(
                before[&track.id],
                (track.artist_id.clone(), track.album_id.clone()),
                "regroup must derive the same ids as the scanner"
            );
        }
    }

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

    // With no folder cover, an album falls back to a track's embedded artwork: its
    // `artwork_path` points at the audio file (the server extracts the picture), and an
    // `artwork_url` is emitted so the UI requests it instead of showing the monogram.
    #[test]
    fn album_falls_back_to_embedded_artwork_when_no_folder_cover() {
        let fixture = TestFixture::new("embedded-fallback");
        fixture.write_rich_tagged_mp3("Album Dir/track.mp3"); // has embedded art, no cover.jpg
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        let track = library.tracks.first().expect("track");
        let album = library.albums.first().expect("album");
        assert_eq!(
            album.artwork_path.as_deref(),
            Some(track.path.as_path()),
            "album artwork falls back to the audio file holding the embedded cover"
        );
        assert!(
            album.artwork_url.is_some(),
            "an artwork_url is emitted so the UI requests the cover"
        );

        // The extractor returns the embedded picture bytes from the same file.
        let bytes = std::fs::read(&track.path).expect("read fixture");
        let cover = extract_embedded_cover(&bytes, &track.path).expect("embedded cover");
        assert_eq!(cover, vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]);
    }

    // A folder cover still wins over embedded art.
    #[test]
    fn folder_cover_preferred_over_embedded() {
        let fixture = TestFixture::new("folder-over-embedded");
        fixture.write_rich_tagged_mp3("Album Dir/track.mp3");
        std::fs::write(fixture.root.join("Album Dir/cover.jpg"), b"folder-cover")
            .expect("write cover");
        let library = scan_local_library(&fixture.root).expect("scan fixture");

        let album = library.albums.first().expect("album");
        assert_eq!(
            album.artwork_path.as_deref().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("cover.jpg")),
            "a real folder cover takes precedence over embedded art"
        );
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
