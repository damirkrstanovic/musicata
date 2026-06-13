//! Upstream **OpenSubsonic client** provider — consume another Subsonic/Navidrome/Gonic/
//! Funkwhale server as a music source (the inverse of [`crate::subsonic`], which *serves* the
//! Subsonic API).
//!
//! Unlike [`crate::smb`] (a `SourceFs` VFS walked by the shared lofty scanner over real files),
//! the remote exposes already-parsed metadata via its REST API. So scanning calls the API
//! (`getAlbumList2` → `getAlbum`) and builds a [`Library`] **directly** — no `SourceFs`, no tag
//! parsing, no downloading audio to scan. We produce [`BuiltTrack`]s and hand them to
//! [`assemble_library`], reusing its stable, de-collided id assignment and album/artist grouping
//! (the same path the local and SMB scanners use). Streaming **proxies** the upstream
//! `/rest/stream`, forwarding the client's `Range` header, so upstream credentials never leave
//! the server (mirrors how [`crate::main`]'s `stream_track` dispatches SMB).

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use musicata_core::{
    Library, MetadataApprovalState, ProviderMapping, ScanIssue, ScanProgress, Track,
    TrackMetadataObservation, album_identity, artist_identity, assemble_library,
};
use musicata_storage::SourceRecord;

/// Subsonic protocol version we advertise as a client. 1.16.1 is the OpenSubsonic baseline.
const PROTOCOL_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "musicata";
/// Subsonic caps `getAlbumList2` at 500 per page; we page until a short page.
const ALBUM_PAGE_SIZE: u32 = 500;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Be polite to the upstream during a big crawl — space request *starts* a touch apart.
const MIN_INTERVAL: Duration = Duration::from_millis(40);

/// Set to force a full re-enumeration (skip the incremental album-count reuse) on the next scan.
const FULL_RESCAN_ENV: &str = "MUSICATA_OPENSUBSONIC_FULL_RESCAN";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Config + auth
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenSubsonicConfig {
    /// Base URL with no trailing slash, e.g. `https://navidrome.example.com`.
    base_url: String,
    username: String,
    password: String,
}

impl OpenSubsonicConfig {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let raw = record
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .ok_or_else(|| anyhow::anyhow!("OpenSubsonic source needs a server URL"))?;
        // Default to https:// when the user typed a bare host.
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        let base_url = with_scheme.trim_end_matches('/').to_string();
        let username = record
            .username
            .as_deref()
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .ok_or_else(|| anyhow::anyhow!("OpenSubsonic source needs a username"))?
            .to_string();
        let password = record
            .password
            .clone()
            .filter(|pw| !pw.is_empty())
            .ok_or_else(|| anyhow::anyhow!("OpenSubsonic source needs a password"))?;
        Ok(Self {
            base_url,
            username,
            password,
        })
    }
}

fn md5_hex(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Response DTOs — match what `crate::subsonic` emits, so the self-hosting test parses for real.
// All fields optional: real servers vary, and missing fields must not fail the whole scan.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "subsonic-response")]
    response: SubResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubResponse {
    status: String,
    error: Option<SubError>,
    album: Option<AlbumDetail>,
    album_list2: Option<AlbumList2>,
}

#[derive(Deserialize)]
struct SubError {
    code: Option<i64>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct AlbumList2 {
    #[serde(default)]
    album: Vec<AlbumRef>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumRef {
    pub id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub song_count: Option<u32>,
}

impl AlbumRef {
    fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("Unknown Album")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumDetail {
    #[allow(dead_code)]
    id: String,
    name: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    year: Option<u16>,
    #[serde(default)]
    song: Vec<SongRef>,
}

impl AlbumDetail {
    fn display_name(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| "Unknown Album".to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SongRef {
    id: String,
    title: Option<String>,
    album: Option<String>,
    artist: Option<String>,
    track: Option<u16>,
    disc_number: Option<u16>,
    year: Option<u16>,
    suffix: Option<String>,
    content_type: Option<String>,
    size: Option<u64>,
    duration: Option<u32>,
    created: Option<String>,
    music_brainz_id: Option<String>,
}

/// Map a `contentType` to a file extension, for servers that omit `suffix`.
fn extension_from_content_type(content_type: &str) -> &'static str {
    match content_type.split(';').next().unwrap_or("").trim() {
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" | "audio/vorbis" | "application/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/aiff" | "audio/x-aiff" => "aiff",
        _ => "mp3",
    }
}

/// Parse an RFC3339 `created` timestamp into Unix seconds (best-effort; `None` if unparseable).
/// Avoids a chrono dependency — handles the `YYYY-MM-DDTHH:MM:SS[.fff]Z` shape Subsonic emits.
fn parse_created(created: Option<&str>) -> Option<i64> {
    let s = created?;
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let time = time.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
    // Days from the civil epoch (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

// ---------------------------------------------------------------------------------------------
// Sync HTTP client (run on spawn_blocking) — mirrors recommendations::ListenBrainzClient.
// ---------------------------------------------------------------------------------------------

pub struct OpenSubsonicClient {
    config: OpenSubsonicConfig,
    agent: ureq::Agent,
    salt: String,
    next_slot: Mutex<Option<Instant>>,
}

impl OpenSubsonicClient {
    pub fn new(config: OpenSubsonicConfig) -> Self {
        let user_agent = format!("Musicata/{}", env!("CARGO_PKG_VERSION"));
        let agent = ureq::AgentBuilder::new()
            .user_agent(&user_agent)
            .timeout(TIMEOUT)
            .build();
        // A fixed per-client salt is allowed by the spec (the token is salt-dependent, not
        // single-use). Derive a stable-but-unguessable hex string without an RNG dependency.
        let salt = md5_hex(&format!("{}:{}", config.username, now_unix()))[..16].to_string();
        Self {
            config,
            agent,
            salt,
            next_slot: Mutex::new(None),
        }
    }

    fn reserve_slot(&self) {
        let start = {
            let mut slot = self.next_slot.lock().expect("opensubsonic limiter poisoned");
            let now = Instant::now();
            let start = match *slot {
                Some(next) if next > now => next,
                _ => now,
            };
            *slot = Some(start + MIN_INTERVAL);
            start
        };
        let now = Instant::now();
        if start > now {
            std::thread::sleep(start - now);
        }
    }

    /// The shared auth + format query params for every request.
    fn auth_params(&self) -> [(&'static str, String); 6] {
        let token = md5_hex(&format!("{}{}", self.config.password, self.salt));
        [
            ("u", self.config.username.clone()),
            ("t", token),
            ("s", self.salt.clone()),
            ("v", PROTOCOL_VERSION.to_string()),
            ("c", CLIENT_NAME.to_string()),
            ("f", "json".to_string()),
        ]
    }

    /// GET `/rest/{view}` with auth + `extra` params, parse the envelope, surface a Subsonic
    /// `failed` status as an `Err`.
    fn get(&self, view: &str, extra: &[(&str, &str)]) -> Result<SubResponse, String> {
        self.reserve_slot();
        let url = format!("{}/rest/{view}", self.config.base_url);
        let mut request = self.agent.get(&url);
        for (key, value) in self.auth_params() {
            request = request.query(key, &value);
        }
        for (key, value) in extra {
            request = request.query(key, value);
        }
        let envelope = request
            .call()
            .map_err(stringify_ureq)?
            .into_json::<Envelope>()
            .map_err(|error| format!("decode {view}: {error}"))?;
        if envelope.response.status != "ok" {
            let error = envelope.response.error;
            let code = error.as_ref().and_then(|e| e.code).unwrap_or(0);
            let message = error
                .and_then(|e| e.message)
                .unwrap_or_else(|| "request failed".to_string());
            return Err(format!("Subsonic error {code}: {message}"));
        }
        Ok(envelope.response)
    }

    /// Liveness + credential check.
    fn ping(&self) -> Result<(), String> {
        self.get("ping", &[]).map(|_| ())
    }

    /// One page of albums; an empty/short page (< `ALBUM_PAGE_SIZE`) means the end.
    fn get_album_list2(&self, offset: u32) -> Result<Vec<AlbumRef>, String> {
        let size = ALBUM_PAGE_SIZE.to_string();
        let offset = offset.to_string();
        let response = self.get(
            "getAlbumList2",
            &[
                ("type", "alphabeticalByName"),
                ("size", &size),
                ("offset", &offset),
            ],
        )?;
        Ok(response.album_list2.map(|list| list.album).unwrap_or_default())
    }

    /// An album with its songs.
    fn get_album(&self, album_id: &str) -> Result<AlbumDetail, String> {
        self.get("getAlbum", &[("id", album_id)])?
            .album
            .ok_or_else(|| format!("getAlbum {album_id}: no album in response"))
    }

    /// The proxied stream request — original bytes (`format=raw`, no transcode), forwarding the
    /// client's `Range` header. Blocking; call on a blocking thread.
    fn open_stream(&self, song_id: &str, range: Option<&str>) -> Result<ureq::Response, String> {
        let url = format!("{}/rest/stream", self.config.base_url);
        let mut request = self.agent.get(&url);
        for (key, value) in self.auth_params() {
            request = request.query(key, &value);
        }
        request = request
            .query("id", song_id)
            .query("format", "raw")
            .query("maxBitRate", "0");
        if let Some(range) = range {
            request = request.set("Range", range);
        }
        request.call().map_err(stringify_ureq)
    }
}

fn stringify_ureq(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let body = response
                .into_string()
                .unwrap_or_else(|_| "<no body>".to_string());
            format!("HTTP {status}: {body}")
        }
        ureq::Error::Transport(error) => error.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Track building
// ---------------------------------------------------------------------------------------------

/// Sanitize one path component for the synthetic `relative_path` (used for folder-browse facets
/// and the deterministic id-assignment sort in `assemble_library`).
fn path_component(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "Unknown".to_string()
    } else {
        trimmed.replace(['/', '\\'], "_")
    }
}

/// Turn one upstream song into a [`BuiltTrack`], reusing the core identity/grouping helpers so it
/// merges and ids exactly like a locally-scanned track.
fn song_to_built(
    song: &SongRef,
    album: &AlbumDetail,
    provider_id: &str,
    observed_at: i64,
) -> musicata_core::BuiltTrack {
    let title = song
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Unknown Title".to_string());
    let artist_name = song
        .artist
        .clone()
        .or_else(|| album.artist.clone())
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let album_artist_name = album
        .artist
        .clone()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| artist_name.clone());
    let album_title = song
        .album
        .clone()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| album.display_name());
    let year = song.year.or(album.year);
    let extension = song
        .suffix
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| {
            song.content_type
                .as_deref()
                .map(extension_from_content_type)
                .unwrap_or("mp3")
                .to_string()
        });

    // Content-keyed ids (no reliable upstream MBID → key on normalized names), so the same album
    // from a local source and the remote merge into one library album.
    let artist_id = artist_identity(&artist_name, None);
    let album_id = album_identity(&album_artist_name, &album_title, None);
    let album_artist_id = artist_identity(&album_artist_name, None);

    // One authoritative observation (treated like an embedded tag by the regroup/enrich passes),
    // carrying the recording MBID when the upstream supplies it so identity can later key on it.
    let recording_mbid = song
        .music_brainz_id
        .clone()
        .filter(|m| !m.trim().is_empty());
    let observation = TrackMetadataObservation {
        source: "embedded_tag".to_string(),
        confidence: 0.9,
        observed_at_unix_seconds: observed_at,
        approval_state: MetadataApprovalState::Observed,
        field_observations: Vec::new(),
        title: Some(title.clone()),
        artist_name: Some(artist_name.clone()),
        album_artist_name: Some(album_artist_name.clone()),
        album_title: Some(album_title.clone()),
        recording_date: None,
        year,
        track_number: song.track,
        track_total: None,
        disc_number: song.disc_number,
        disc_total: None,
        genres: Vec::new(),
        composers: Vec::new(),
        lyrics: None,
        musicbrainz_recording_id: recording_mbid,
        musicbrainz_track_id: None,
        musicbrainz_release_id: None,
        musicbrainz_release_group_id: None,
        musicbrainz_artist_id: None,
        musicbrainz_release_artist_id: None,
        isrc: None,
        embedded_artwork_count: 0,
    };

    let relative_path = format!(
        "{}/{}/{}.{}",
        path_component(&album_artist_name),
        path_component(&album_title),
        path_component(&title),
        extension
    );

    let track = Track {
        id: String::new(), // assigned (deduped) in assemble_library
        provider: ProviderMapping {
            provider_id: provider_id.to_string(),
            item_id: song.id.clone(),
        },
        observed_metadata: vec![observation],
        title,
        artist_id,
        artist_name,
        album_id,
        album_title,
        year,
        track_number: song.track,
        disc_number: song.disc_number,
        extension,
        file_size_bytes: song.size,
        duration_seconds: song.duration.map(|d| d as f64),
        modified_at_unix_seconds: parse_created(song.created.as_deref()),
        content_hash: None,
        relative_path,
        stream_url: String::new(), // assigned in assemble_library
        added_at_unix_seconds: None,
        path: std::path::PathBuf::new(), // unused; streaming keys on provider.item_id
    };

    musicata_core::BuiltTrack {
        track,
        // The remote song id makes the identity stable across rescans and distinct from local
        // tracks (whose identity is content-hash/size based); genuine cross-source duplicates
        // still merge-suffix in `merge_libraries`.
        identity: Some(format!("opensubsonic:{provider_id}:{}", song.id)),
        album_artist_id,
        album_artist_name,
        artwork_path: None, // filled later by the artwork-acquisition pass, keyed on album_id
        issues: Vec::new(),
        io_error: false,
    }
}

/// Decide, per upstream album, whether its prior tracks can be reused (same song count) or it
/// must be fetched. Pure (no HTTP) so it's unit-testable. `prior_counts` maps a content
/// `album_id` to how many prior tracks this provider had for it.
struct AlbumScanPlan {
    /// content album_ids whose prior tracks we reuse wholesale.
    reuse: Vec<String>,
    /// upstream album ids we must `getAlbum`.
    fetch: Vec<String>,
}

fn plan_album_fetches(prior_counts: &HashMap<String, usize>, albums: &[AlbumRef]) -> AlbumScanPlan {
    let mut reuse = Vec::new();
    let mut fetch = Vec::new();
    for album in albums {
        let content_id = album
            .artist
            .as_deref()
            .filter(|a| !a.trim().is_empty())
            .map(|artist| album_identity(artist, album.display_name(), None));
        match (content_id, album.song_count) {
            (Some(content_id), Some(count))
                if count > 0 && prior_counts.get(&content_id) == Some(&(count as usize)) =>
            {
                reuse.push(content_id)
            }
            _ => fetch.push(album.id.clone()),
        }
    }
    AlbumScanPlan { reuse, fetch }
}

// ---------------------------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------------------------

pub struct OpenSubsonicProvider {
    provider_id: String,
    source_root: String,
    client: Arc<OpenSubsonicClient>,
}

impl OpenSubsonicProvider {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let config = OpenSubsonicConfig::from_record(record)?;
        let source_root = config.base_url.clone();
        Ok(Self {
            provider_id: record.id.clone(),
            source_root,
            client: Arc::new(OpenSubsonicClient::new(config)),
        })
    }

    pub fn provider_id(&self) -> &String {
        &self.provider_id
    }

    /// Reachability + credential check (a `ping`).
    pub async fn validate(&self) -> anyhow::Result<()> {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || client.ping())
            .await?
            .map_err(|error| anyhow::anyhow!("OpenSubsonic server unreachable: {error}"))
    }

    /// Enumerate the remote catalogue into a [`Library`]. Incremental: an album whose upstream
    /// `songCount` matches the count of prior tracks we already hold is reused wholesale (no
    /// `getAlbum`), so a steady-state rescan is O(album-list pages) rather than O(albums).
    pub async fn scan_with_progress<F>(
        &self,
        prior: Option<Arc<Library>>,
        mut progress: F,
    ) -> anyhow::Result<Library>
    where
        F: FnMut(ScanProgress) + Send + 'static,
    {
        let client = self.client.clone();
        let provider_id = self.provider_id.clone();
        let source_root = self.source_root.clone();
        let full_rescan = std::env::var(FULL_RESCAN_ENV).is_ok();

        // Prior tracks for this provider, grouped by content album_id (for the incremental reuse)
        // and the full track list (to clone reused tracks). Computed here (cheap) and moved in.
        let mut prior_counts: HashMap<String, usize> = HashMap::new();
        let mut prior_by_album: HashMap<String, Vec<Track>> = HashMap::new();
        if !full_rescan && let Some(prior) = prior.as_deref() {
            for track in &prior.tracks {
                if track.provider.provider_id == provider_id {
                    *prior_counts.entry(track.album_id.clone()).or_default() += 1;
                    prior_by_album
                        .entry(track.album_id.clone())
                        .or_default()
                        .push(track.clone());
                }
            }
        }

        progress(ScanProgress::Discovering { found: 0 });

        let scanned = tokio::task::spawn_blocking(move || -> anyhow::Result<Library> {
            // 1. Page the album list (cheap — a handful of calls).
            let mut albums: Vec<AlbumRef> = Vec::new();
            let mut offset = 0u32;
            loop {
                let page = client
                    .get_album_list2(offset)
                    .map_err(|error| anyhow::anyhow!("getAlbumList2: {error}"))?;
                let page_len = page.len() as u32;
                albums.extend(page);
                progress(ScanProgress::Discovering {
                    found: albums.len(),
                });
                if page_len < ALBUM_PAGE_SIZE {
                    break;
                }
                offset += ALBUM_PAGE_SIZE;
            }

            // 2. Plan reuse vs fetch.
            let plan = plan_album_fetches(&prior_counts, &albums);
            let total_albums = plan.reuse.len() + plan.fetch.len();
            let estimated_songs: usize = albums
                .iter()
                .filter_map(|a| a.song_count)
                .map(|c| c as usize)
                .sum();
            progress(ScanProgress::Discovered {
                files: estimated_songs,
            });

            let observed_at = now_unix();
            let mut built: Vec<musicata_core::BuiltTrack> = Vec::new();
            let mut scan_errors: Vec<ScanIssue> = Vec::new();

            // 3a. Reuse unchanged albums' prior tracks wholesale (identity: None → keep ids).
            for content_id in &plan.reuse {
                if let Some(tracks) = prior_by_album.get(content_id) {
                    for track in tracks {
                        let album_artist = track.artist_name.clone();
                        built.push(musicata_core::BuiltTrack {
                            track: track.clone(),
                            identity: None,
                            album_artist_id: track.artist_id.clone(),
                            album_artist_name: album_artist,
                            artwork_path: None,
                            issues: Vec::new(),
                            io_error: false,
                        });
                    }
                }
            }

            // 3b. Fetch new/changed albums.
            let mut done_albums = plan.reuse.len();
            let mut last_report = Instant::now();
            for album_id in &plan.fetch {
                match client.get_album(album_id) {
                    Ok(album) => {
                        for song in &album.song {
                            built.push(song_to_built(song, &album, &provider_id, observed_at));
                        }
                    }
                    Err(error) => scan_errors.push(ScanIssue {
                        path: format!("album:{album_id}"),
                        message: error,
                    }),
                }
                done_albums += 1;
                if last_report.elapsed() >= Duration::from_millis(300)
                    || done_albums == total_albums
                {
                    progress(ScanProgress::Processing {
                        done: built.len(),
                        total: estimated_songs.max(built.len()),
                        detail: Some(format!("{done_albums}/{total_albums} albums")),
                    });
                    last_report = Instant::now();
                }
            }

            Ok(assemble_library(
                &provider_id,
                std::path::Path::new(&source_root),
                built,
                scan_errors,
            ))
        })
        .await??;

        Ok(scanned)
    }

    /// Proxy the upstream stream for `item_id` (a remote song id), forwarding `range`. Returns the
    /// upstream status, the headers worth passing through, and a chunked body stream (read on a
    /// blocking thread into a small bounded channel — backpressure, no whole-file buffering).
    pub async fn read_range_stream(
        &self,
        item_id: &str,
        range: Option<String>,
    ) -> anyhow::Result<(
        u16,
        Vec<(String, String)>,
        ReceiverStream<std::io::Result<Vec<u8>>>,
    )> {
        let client = self.client.clone();
        let song_id = item_id.to_string();
        let response = tokio::task::spawn_blocking(move || {
            client.open_stream(&song_id, range.as_deref())
        })
        .await?
        .map_err(|error| anyhow::anyhow!("upstream stream: {error}"))?;

        let status = response.status();
        let mut headers = Vec::new();
        for name in ["Content-Type", "Content-Length", "Content-Range", "Accept-Ranges"] {
            if let Some(value) = response.header(name) {
                headers.push((name.to_string(), value.to_string()));
            }
        }

        let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(4);
        let mut reader = response.into_reader();
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if tx.blocking_send(Ok(buffer[..read].to_vec())).is_err() {
                            break; // client went away
                        }
                    }
                    Err(error) => {
                        let _ = tx.blocking_send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok((status, headers, ReceiverStream::new(rx)))
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_token_matches_subsonic_vector() {
        // The canonical Subsonic example: password "sesame", salt "c19b2d" → known token.
        assert_eq!(
            md5_hex("sesamec19b2d"),
            "26719a1196d2a940705a59634eb18eab"
        );
    }

    #[test]
    fn parses_get_album_envelope() {
        let json = r#"{
          "subsonic-response": {
            "status": "ok", "version": "1.16.1",
            "album": {
              "id": "al-1", "name": "Random Access Memories", "artist": "Daft Punk",
              "artistId": "ar-1", "songCount": 2, "year": 2013,
              "song": [
                {"id":"s1","title":"Giorgio by Moroder","album":"Random Access Memories",
                 "artist":"Daft Punk","track":3,"year":2013,"suffix":"flac",
                 "contentType":"audio/flac","size":92160000,"duration":544,
                 "created":"2013-05-17T00:00:00.000Z","musicBrainzId":"mbid-1"},
                {"id":"s2","title":"Get Lucky","artist":"Daft Punk","track":8,"suffix":"mp3",
                 "size":8200000,"duration":369}
              ]
            }
          }
        }"#;
        let env: Envelope = serde_json::from_str(json).expect("parse");
        assert_eq!(env.response.status, "ok");
        let album = env.response.album.expect("album");
        assert_eq!(album.display_name(), "Random Access Memories");
        assert_eq!(album.song.len(), 2);
        assert_eq!(album.song[0].suffix.as_deref(), Some("flac"));
        assert_eq!(album.song[0].duration, Some(544));
        assert_eq!(album.song[1].id, "s2");
    }

    #[test]
    fn parses_failed_envelope() {
        let json = r#"{"subsonic-response":{"status":"failed","version":"1.16.1",
            "error":{"code":40,"message":"Wrong username or password."}}}"#;
        let env: Envelope = serde_json::from_str(json).expect("parse");
        assert_eq!(env.response.status, "failed");
        assert_eq!(env.response.error.unwrap().code, Some(40));
    }

    #[test]
    fn extension_falls_back_to_content_type() {
        assert_eq!(extension_from_content_type("audio/flac"), "flac");
        assert_eq!(extension_from_content_type("audio/mpeg"), "mp3");
        assert_eq!(extension_from_content_type("audio/mp4; codecs=mp4a"), "m4a");
        assert_eq!(extension_from_content_type("application/octet-stream"), "mp3");
    }

    #[test]
    fn parse_created_to_unix() {
        // 2013-05-17T00:00:00Z = 1368748800.
        assert_eq!(parse_created(Some("2013-05-17T00:00:00.000Z")), Some(1368748800));
        assert_eq!(parse_created(Some("1970-01-01T00:00:01Z")), Some(1));
        assert_eq!(parse_created(None), None);
        assert_eq!(parse_created(Some("garbage")), None);
    }

    fn album_ref(id: &str, artist: &str, name: &str, count: u32) -> AlbumRef {
        AlbumRef {
            id: id.into(),
            name: Some(name.into()),
            title: None,
            artist: Some(artist.into()),
            song_count: Some(count),
        }
    }

    #[test]
    fn incremental_reuses_unchanged_and_fetches_changed() {
        // Prior: album A had 3 tracks, album B had 2 tracks (by content id).
        let a_id = album_identity("Daft Punk", "Discovery", None);
        let b_id = album_identity("Justice", "Cross", None);
        let mut prior = HashMap::new();
        prior.insert(a_id.clone(), 3usize);
        prior.insert(b_id.clone(), 2usize);

        let albums = vec![
            album_ref("rA", "Daft Punk", "Discovery", 3), // unchanged → reuse
            album_ref("rB", "Justice", "Cross", 4),       // grew → fetch
            album_ref("rC", "Air", "Moon Safari", 10),    // new → fetch
        ];
        let plan = plan_album_fetches(&prior, &albums);
        assert_eq!(plan.reuse, vec![a_id]);
        assert_eq!(plan.fetch, vec!["rB".to_string(), "rC".to_string()]);
    }

    #[test]
    fn incremental_fetches_when_artist_missing() {
        // No artist → can't compute a content id → must fetch.
        let albums = vec![AlbumRef {
            id: "rX".into(),
            name: Some("Mystery".into()),
            title: None,
            artist: None,
            song_count: Some(5),
        }];
        let plan = plan_album_fetches(&HashMap::new(), &albums);
        assert!(plan.reuse.is_empty());
        assert_eq!(plan.fetch, vec!["rX".to_string()]);
    }

    /// Live smoke test against a real OpenSubsonic server. `#[ignore]`d so default `cargo test`
    /// stays offline; run with the env vars set:
    ///   MUSICATA_OS_URL=https://… MUSICATA_OS_USER=… MUSICATA_OS_PASS=… \
    ///     cargo test -p musicata-server --bins -- --ignored opensubsonic_live
    #[test]
    #[ignore = "hits a live OpenSubsonic server (set MUSICATA_OS_URL/USER/PASS)"]
    fn opensubsonic_live_path() {
        let config = OpenSubsonicConfig {
            base_url: std::env::var("MUSICATA_OS_URL")
                .expect("MUSICATA_OS_URL")
                .trim_end_matches('/')
                .to_string(),
            username: std::env::var("MUSICATA_OS_USER").expect("MUSICATA_OS_USER"),
            password: std::env::var("MUSICATA_OS_PASS").expect("MUSICATA_OS_PASS"),
        };
        let client = OpenSubsonicClient::new(config);
        client.ping().expect("ping");
        let albums = client.get_album_list2(0).expect("getAlbumList2");
        assert!(!albums.is_empty(), "expected at least one album");
        let detail = client.get_album(&albums[0].id).expect("getAlbum");
        assert!(!detail.song.is_empty(), "expected songs in the first album");
    }
}
