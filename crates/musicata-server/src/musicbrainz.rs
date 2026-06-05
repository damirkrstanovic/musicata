use musicata_core::{Album, Track};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DEFAULT_MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org/ws/2";
const DEFAULT_COVER_ART_ARCHIVE_BASE_URL: &str = "https://coverartarchive.org";
const MUSICBRAINZ_TIMEOUT: Duration = Duration::from_secs(10);
const MUSICBRAINZ_REQUEST_INTERVAL: Duration = Duration::from_millis(1100);

#[derive(Clone)]
pub struct MusicBrainzClient {
    base_url: String,
    cover_art_archive_base_url: String,
    http: ureq::Agent,
}

impl Default for MusicBrainzClient {
    fn default() -> Self {
        Self::new(DEFAULT_MUSICBRAINZ_BASE_URL)
    }
}

impl MusicBrainzClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let user_agent = format!(
            "Musicata/{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        );
        let http = ureq::AgentBuilder::new()
            .user_agent(&user_agent)
            .timeout(MUSICBRAINZ_TIMEOUT)
            .build();

        Self {
            base_url: base_url.into(),
            cover_art_archive_base_url: DEFAULT_COVER_ART_ARCHIVE_BASE_URL.to_string(),
            http,
        }
    }

    pub fn lookup_track(&self, track: &Track) -> MusicBrainzTrackLookupResponse {
        let targets = musicbrainz_lookup_targets(track, &self.base_url);
        let mut lookup = MusicBrainzTrackLookupResponse {
            track_id: track.id.clone(),
            targets: targets.clone(),
            recordings: Vec::new(),
            tracks: Vec::new(),
            releases: Vec::new(),
            release_groups: Vec::new(),
            artists: Vec::new(),
            issues: Vec::new(),
        };

        for (index, target) in targets.into_iter().enumerate() {
            if index > 0 {
                std::thread::sleep(MUSICBRAINZ_REQUEST_INTERVAL);
            }

            match self.fetch_target(&target) {
                Ok(document) => lookup.push_document(document),
                Err(message) => lookup.issues.push(MusicBrainzLookupIssue {
                    entity_type: target.entity_type,
                    mbid: target.mbid,
                    message,
                }),
            }
        }

        lookup
    }

    pub fn search_track_candidates(
        &self,
        track: &Track,
        limit: usize,
    ) -> MusicBrainzTrackCandidateSearchResponse {
        let limit = limit.clamp(1, 25);
        let existing_targets = musicbrainz_lookup_targets(track, &self.base_url);
        let query = musicbrainz_track_candidate_query(track);
        let search_url = musicbrainz_search_url(
            &self.base_url,
            MusicBrainzSearchEntityType::Recording,
            &query,
            limit,
        );
        let mut response = MusicBrainzTrackCandidateSearchResponse {
            track_id: track.id.clone(),
            query,
            search_url,
            searched: false,
            skipped_reason: None,
            candidates: Vec::new(),
            issues: Vec::new(),
        };

        if !existing_targets.is_empty() {
            response.skipped_reason = Some("track already has MusicBrainz identifiers".to_string());
            return response;
        }

        if response.query.is_empty() {
            response.issues.push(MusicBrainzSearchIssue {
                entity_type: MusicBrainzSearchEntityType::Recording,
                message: "track has no searchable metadata".to_string(),
            });
            return response;
        }

        response.searched = true;
        match self.fetch_search(
            MusicBrainzSearchEntityType::Recording,
            &response.query,
            limit,
        ) {
            Ok(value) => {
                response.candidates = recording_candidates(&value);
            }
            Err(message) => response.issues.push(MusicBrainzSearchIssue {
                entity_type: MusicBrainzSearchEntityType::Recording,
                message,
            }),
        }

        response
    }

    pub fn search_album_candidates(
        &self,
        album: &Album,
        tracks: &[Track],
        limit: usize,
    ) -> MusicBrainzAlbumCandidateSearchResponse {
        let limit = limit.clamp(1, 25);
        let query = musicbrainz_album_candidate_query(album);
        let search_url = musicbrainz_search_url(
            &self.base_url,
            MusicBrainzSearchEntityType::Release,
            &query,
            limit,
        );
        let mut response = MusicBrainzAlbumCandidateSearchResponse {
            album_id: album.id.clone(),
            query,
            search_url,
            searched: false,
            skipped_reason: None,
            candidates: Vec::new(),
            issues: Vec::new(),
        };

        if album_has_existing_release_mbid(tracks) {
            response.skipped_reason =
                Some("album already has MusicBrainz release identifiers".to_string());
            return response;
        }

        if response.query.is_empty() {
            response.issues.push(MusicBrainzSearchIssue {
                entity_type: MusicBrainzSearchEntityType::Release,
                message: "album has no searchable metadata".to_string(),
            });
            return response;
        }

        response.searched = true;
        match self.fetch_search(MusicBrainzSearchEntityType::Release, &response.query, limit) {
            Ok(value) => {
                response.candidates = release_candidates(&value);
            }
            Err(message) => response.issues.push(MusicBrainzSearchIssue {
                entity_type: MusicBrainzSearchEntityType::Release,
                message,
            }),
        }

        response
    }

    pub fn cover_art_archive_candidates(
        &self,
        album: &Album,
        tracks: &[Track],
        limit: usize,
    ) -> CoverArtArchiveCandidateResponse {
        let limit = limit.clamp(1, 25);
        let targets = cover_art_archive_targets(tracks, &self.cover_art_archive_base_url);
        let mut response = CoverArtArchiveCandidateResponse {
            album_id: album.id.clone(),
            targets: targets.clone(),
            searched: false,
            skipped_reason: None,
            candidates: Vec::new(),
            issues: Vec::new(),
        };

        if targets.is_empty() {
            response.skipped_reason =
                Some("album has no MusicBrainz release or release-group identifiers".to_string());
            return response;
        }

        response.searched = true;
        for (index, target) in targets.into_iter().take(limit).enumerate() {
            if index > 0 {
                std::thread::sleep(MUSICBRAINZ_REQUEST_INTERVAL);
            }

            match self.fetch_cover_art_archive(&target) {
                Ok(mut candidates) => response.candidates.append(&mut candidates),
                Err(message) => response
                    .issues
                    .push(CoverArtArchiveIssue { target, message }),
            }
        }

        response
    }

    fn fetch_target(
        &self,
        target: &MusicBrainzLookupTarget,
    ) -> Result<MusicBrainzDocument, String> {
        let url = musicbrainz_entity_url(&self.base_url, target.entity_type, &target.mbid);
        let mut request = self.http.get(&url).query("fmt", "json");
        if let Some(include) = target.entity_type.includes() {
            request = request.query("inc", include);
        }

        let response = request.call().map_err(musicbrainz_request_error)?;
        let value = response
            .into_json::<Value>()
            .map_err(|error| error.to_string())?;
        normalize_musicbrainz_document(target, &value)
    }

    /// Background text-search fallback: find the best-matching recording for a track by its
    /// tags (title/artist/album), returning the top candidate's MBIDs + score so the caller
    /// can confidence-gate it. Used for tracks AcoustID can't fingerprint-match.
    pub fn search_recording(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        year: Option<u16>,
    ) -> Result<Option<RecordingSearchMatch>, String> {
        let query = recording_search_query(title, artist, album, year);
        if query.is_empty() {
            return Ok(None);
        }
        // `Err` = a transport/HTTP failure (caller retries); `Ok(None)` = no candidate.
        let value = self.fetch_search(MusicBrainzSearchEntityType::Recording, &query, 5)?;
        let best = recording_candidates(&value)
            .into_iter()
            .max_by_key(|candidate| candidate.score.unwrap_or(0));
        Ok(best.map(|best| RecordingSearchMatch {
            recording_mbid: best.id,
            release_mbid: best.releases.first().map(|release| release.id.clone()),
            score: best.score.unwrap_or(0),
            artist: best.artist_credit.join(", "),
            title: best.title.unwrap_or_default(),
        }))
    }

    fn fetch_search(
        &self,
        entity_type: MusicBrainzSearchEntityType,
        query: &str,
        limit: usize,
    ) -> Result<Value, String> {
        let url = musicbrainz_search_endpoint(&self.base_url, entity_type);
        let response = self
            .http
            .get(&url)
            .query("fmt", "json")
            .query("query", query)
            .query("limit", &limit.to_string())
            .call()
            .map_err(musicbrainz_request_error)?;

        response
            .into_json::<Value>()
            .map_err(|error| error.to_string())
    }

    fn fetch_cover_art_archive(
        &self,
        target: &CoverArtArchiveTarget,
    ) -> Result<Vec<CoverArtArchiveCandidate>, String> {
        let response = self
            .http
            .get(&target.lookup_url)
            .call()
            .map_err(cover_art_archive_request_error)?;
        let value = response
            .into_json::<Value>()
            .map_err(|error| error.to_string())?;

        Ok(cover_art_archive_candidates(target, &value))
    }

    /// Fetch real metadata for a fingerprinted recording: the recording's title and
    /// artist, plus — when a release MBID is known — the album, album artist, release
    /// date, and the track's position within that release (track/disc number). Two
    /// rate-limited requests; used by the background enrichment pass. Pure parsing lives
    /// in [`parse_recording_enrichment`] / [`merge_release_enrichment`] (canned-JSON
    /// tested).
    pub fn fetch_enrichment(
        &self,
        recording_mbid: &str,
        release_mbid: Option<&str>,
    ) -> Result<MusicBrainzEnrichment, String> {
        let recording_url =
            musicbrainz_entity_url(&self.base_url, MusicBrainzEntityType::Recording, recording_mbid);
        let recording = self
            .http
            .get(&recording_url)
            .query("fmt", "json")
            .query("inc", "artist-credits")
            .call()
            .map_err(musicbrainz_request_error)?
            .into_json::<Value>()
            .map_err(|error| error.to_string())?;
        let mut enrichment = parse_recording_enrichment(&recording);

        if let Some(release_mbid) = release_mbid {
            std::thread::sleep(MUSICBRAINZ_REQUEST_INTERVAL);
            let release_url =
                musicbrainz_entity_url(&self.base_url, MusicBrainzEntityType::Release, release_mbid);
            let release = self
                .http
                .get(&release_url)
                .query("fmt", "json")
                .query("inc", "artist-credits+release-groups+media+recordings")
                .call()
                .map_err(musicbrainz_request_error)?
                .into_json::<Value>()
                .map_err(|error| error.to_string())?;
            merge_release_enrichment(&mut enrichment, &release, recording_mbid);
        }
        Ok(enrichment)
    }
}

fn musicbrainz_request_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("MusicBrainz returned HTTP {status}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}

fn cover_art_archive_request_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("Cover Art Archive returned HTTP {status}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzTrackLookupResponse {
    pub track_id: String,
    pub targets: Vec<MusicBrainzLookupTarget>,
    pub recordings: Vec<MusicBrainzRecording>,
    pub tracks: Vec<MusicBrainzTrack>,
    pub releases: Vec<MusicBrainzRelease>,
    pub release_groups: Vec<MusicBrainzReleaseGroup>,
    pub artists: Vec<MusicBrainzArtist>,
    pub issues: Vec<MusicBrainzLookupIssue>,
}

impl MusicBrainzTrackLookupResponse {
    fn push_document(&mut self, document: MusicBrainzDocument) {
        match document {
            MusicBrainzDocument::Recording(recording) => self.recordings.push(recording),
            MusicBrainzDocument::Track(track) => self.tracks.push(track),
            MusicBrainzDocument::Release(release) => self.releases.push(release),
            MusicBrainzDocument::ReleaseGroup(release_group) => {
                self.release_groups.push(release_group);
            }
            MusicBrainzDocument::Artist(artist) => self.artists.push(artist),
        }
    }
}

/// Real metadata resolved for a fingerprinted track, fed into the canonical library by
/// the enrichment pass. Numbers are `i64` for ease of SQLite storage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicBrainzEnrichment {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub artist_mbid: Option<String>,
    pub album_title: Option<String>,
    pub album_artist_name: Option<String>,
    pub release_group_mbid: Option<String>,
    pub recording_date: Option<String>,
    pub year: Option<i64>,
    pub track_number: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_number: Option<i64>,
}

impl MusicBrainzEnrichment {
    /// True when nothing usable was resolved (so the pass records `not_found`).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzTrackCandidateSearchResponse {
    pub track_id: String,
    pub query: String,
    pub search_url: String,
    pub searched: bool,
    pub skipped_reason: Option<String>,
    pub candidates: Vec<MusicBrainzRecordingCandidate>,
    pub issues: Vec<MusicBrainzSearchIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzAlbumCandidateSearchResponse {
    pub album_id: String,
    pub query: String,
    pub search_url: String,
    pub searched: bool,
    pub skipped_reason: Option<String>,
    pub candidates: Vec<MusicBrainzReleaseCandidate>,
    pub issues: Vec<MusicBrainzSearchIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzRecordingCandidate {
    pub id: String,
    pub score: Option<u64>,
    pub title: Option<String>,
    pub disambiguation: Option<String>,
    pub length_ms: Option<u64>,
    pub first_release_date: Option<String>,
    pub artist_credit: Vec<String>,
    pub isrcs: Vec<String>,
    pub releases: Vec<MusicBrainzLinkedRelease>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzReleaseCandidate {
    pub id: String,
    pub score: Option<u64>,
    pub title: Option<String>,
    pub disambiguation: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub barcode: Option<String>,
    pub artist_credit: Vec<String>,
    pub release_group: Option<MusicBrainzLinkedReleaseGroup>,
    pub media: Vec<MusicBrainzMedium>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CoverArtArchiveCandidateResponse {
    pub album_id: String,
    pub targets: Vec<CoverArtArchiveTarget>,
    pub searched: bool,
    pub skipped_reason: Option<String>,
    pub candidates: Vec<CoverArtArchiveCandidate>,
    pub issues: Vec<CoverArtArchiveIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CoverArtArchiveTarget {
    pub entity_type: CoverArtArchiveEntityType,
    pub mbid: String,
    pub lookup_url: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverArtArchiveEntityType {
    Release,
    ReleaseGroup,
}

impl CoverArtArchiveEntityType {
    fn path(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::ReleaseGroup => "release-group",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CoverArtArchiveCandidate {
    pub id: String,
    pub entity_type: CoverArtArchiveEntityType,
    pub mbid: String,
    pub image_url: String,
    pub thumbnail_url: Option<String>,
    pub front: bool,
    pub back: bool,
    pub approved: bool,
    pub comment: Option<String>,
    pub edit_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CoverArtArchiveIssue {
    pub target: CoverArtArchiveTarget,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicBrainzSearchEntityType {
    Recording,
    Release,
}

impl MusicBrainzSearchEntityType {
    fn path(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Release => "release",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzSearchIssue {
    pub entity_type: MusicBrainzSearchEntityType,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicBrainzEntityType {
    Recording,
    Track,
    Release,
    ReleaseGroup,
    Artist,
}

impl MusicBrainzEntityType {
    fn path(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Track => "track",
            Self::Release => "release",
            Self::ReleaseGroup => "release-group",
            Self::Artist => "artist",
        }
    }

    fn includes(self) -> Option<&'static str> {
        match self {
            Self::Recording => Some("artist-credits+releases+isrcs"),
            Self::Track => Some("artist-credits+recordings+releases"),
            Self::Release => Some("artist-credits+release-groups+media+recordings+isrcs+labels"),
            Self::ReleaseGroup => Some("artist-credits+releases"),
            Self::Artist => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzLookupTarget {
    pub entity_type: MusicBrainzEntityType,
    pub mbid: String,
    pub lookup_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzLookupIssue {
    pub entity_type: MusicBrainzEntityType,
    pub mbid: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzRecording {
    pub id: String,
    pub title: Option<String>,
    pub disambiguation: Option<String>,
    pub length_ms: Option<u64>,
    pub first_release_date: Option<String>,
    pub artist_credit: Vec<String>,
    pub isrcs: Vec<String>,
    pub releases: Vec<MusicBrainzLinkedRelease>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzTrack {
    pub id: String,
    pub title: Option<String>,
    pub number: Option<String>,
    pub length_ms: Option<u64>,
    pub artist_credit: Vec<String>,
    pub recording: Option<MusicBrainzLinkedRecording>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzRelease {
    pub id: String,
    pub title: Option<String>,
    pub disambiguation: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub barcode: Option<String>,
    pub artist_credit: Vec<String>,
    pub release_group: Option<MusicBrainzLinkedReleaseGroup>,
    pub media: Vec<MusicBrainzMedium>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzReleaseGroup {
    pub id: String,
    pub title: Option<String>,
    pub disambiguation: Option<String>,
    pub first_release_date: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub artist_credit: Vec<String>,
    pub releases: Vec<MusicBrainzLinkedRelease>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzArtist {
    pub id: String,
    pub name: Option<String>,
    pub sort_name: Option<String>,
    pub disambiguation: Option<String>,
    pub artist_type: Option<String>,
    pub country: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzLinkedRecording {
    pub id: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzLinkedRelease {
    pub id: String,
    pub title: Option<String>,
    pub date: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzLinkedReleaseGroup {
    pub id: String,
    pub title: Option<String>,
    pub primary_type: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MusicBrainzMedium {
    pub position: Option<u64>,
    pub format: Option<String>,
    pub track_count: Option<u64>,
}

enum MusicBrainzDocument {
    Recording(MusicBrainzRecording),
    Track(MusicBrainzTrack),
    Release(MusicBrainzRelease),
    ReleaseGroup(MusicBrainzReleaseGroup),
    Artist(MusicBrainzArtist),
}

pub fn musicbrainz_lookup_targets(track: &Track, base_url: &str) -> Vec<MusicBrainzLookupTarget> {
    let mut ids: BTreeMap<MusicBrainzEntityType, BTreeSet<String>> = BTreeMap::new();

    for observation in &track.observed_metadata {
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::Recording,
            observation.musicbrainz_recording_id.as_deref(),
        );
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::Track,
            observation.musicbrainz_track_id.as_deref(),
        );
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::Release,
            observation.musicbrainz_release_id.as_deref(),
        );
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::ReleaseGroup,
            observation.musicbrainz_release_group_id.as_deref(),
        );
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::Artist,
            observation.musicbrainz_artist_id.as_deref(),
        );
        insert_musicbrainz_ids(
            &mut ids,
            MusicBrainzEntityType::Artist,
            observation.musicbrainz_release_artist_id.as_deref(),
        );
    }

    ids.into_iter()
        .flat_map(|(entity_type, ids)| {
            ids.into_iter().map(move |mbid| MusicBrainzLookupTarget {
                entity_type,
                lookup_url: musicbrainz_entity_url(base_url, entity_type, &mbid),
                mbid,
            })
        })
        .collect()
}

pub fn cover_art_archive_targets(tracks: &[Track], base_url: &str) -> Vec<CoverArtArchiveTarget> {
    let mut ids: BTreeMap<CoverArtArchiveEntityType, BTreeSet<String>> = BTreeMap::new();

    for track in tracks {
        for observation in &track.observed_metadata {
            insert_cover_art_archive_ids(
                &mut ids,
                CoverArtArchiveEntityType::Release,
                observation.musicbrainz_release_id.as_deref(),
            );
            insert_cover_art_archive_ids(
                &mut ids,
                CoverArtArchiveEntityType::ReleaseGroup,
                observation.musicbrainz_release_group_id.as_deref(),
            );
        }
    }

    ids.into_iter()
        .flat_map(|(entity_type, ids)| {
            ids.into_iter().map(move |mbid| CoverArtArchiveTarget {
                entity_type,
                lookup_url: cover_art_archive_url(base_url, entity_type, &mbid),
                mbid,
            })
        })
        .collect()
}

pub fn musicbrainz_track_candidate_query(track: &Track) -> String {
    recording_search_query(
        &track.title,
        &track.artist_name,
        &track.album_title,
        track.year,
    )
}

/// The top MusicBrainz recording match for a text search, with enough to confidence-gate it.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingSearchMatch {
    pub recording_mbid: String,
    pub release_mbid: Option<String>,
    /// MusicBrainz relevance score, 0–100.
    pub score: u64,
    pub artist: String,
    pub title: String,
}

/// Build a MusicBrainz recording-search query from track tags.
fn recording_search_query(title: &str, artist: &str, album: &str, year: Option<u16>) -> String {
    let mut parts = Vec::new();
    push_musicbrainz_query_field(&mut parts, "recording", title);
    push_musicbrainz_query_field(&mut parts, "artist", artist);
    push_musicbrainz_query_field(&mut parts, "release", album);
    if let Some(year) = year {
        parts.push(format!("date:{year}"));
    }
    parts.join(" AND ")
}

pub fn musicbrainz_album_candidate_query(album: &Album) -> String {
    let mut parts = Vec::new();

    push_musicbrainz_query_field(&mut parts, "release", &album.title);
    push_musicbrainz_query_field(&mut parts, "artist", &album.artist_name);
    if let Some(year) = album.year {
        parts.push(format!("date:{year}"));
    }

    parts.join(" AND ")
}

fn push_musicbrainz_query_field(parts: &mut Vec<String>, field: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() || value == "Unknown Artist" || value == "Unknown Album" {
        return;
    }

    parts.push(format!("{field}:\"{}\"", escape_musicbrainz_query(value)));
}

fn escape_musicbrainz_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn album_has_existing_release_mbid(tracks: &[Track]) -> bool {
    tracks.iter().any(|track| {
        track.observed_metadata.iter().any(|observation| {
            observation.musicbrainz_release_id.is_some()
                || observation.musicbrainz_release_group_id.is_some()
        })
    })
}

fn musicbrainz_search_endpoint(base_url: &str, entity_type: MusicBrainzSearchEntityType) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), entity_type.path())
}

fn musicbrainz_search_url(
    base_url: &str,
    entity_type: MusicBrainzSearchEntityType,
    query: &str,
    limit: usize,
) -> String {
    format!(
        "{}?query={}&fmt=json&limit={limit}",
        musicbrainz_search_endpoint(base_url, entity_type),
        percent_encode(query)
    )
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();

    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte))
            }
            b' ' => encoded.push_str("%20"),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }

    encoded
}

fn insert_musicbrainz_ids(
    ids: &mut BTreeMap<MusicBrainzEntityType, BTreeSet<String>>,
    entity_type: MusicBrainzEntityType,
    value: Option<&str>,
) {
    let Some(value) = value else {
        return;
    };

    for mbid in split_musicbrainz_ids(value) {
        ids.entry(entity_type).or_default().insert(mbid);
    }
}

fn insert_cover_art_archive_ids(
    ids: &mut BTreeMap<CoverArtArchiveEntityType, BTreeSet<String>>,
    entity_type: CoverArtArchiveEntityType,
    value: Option<&str>,
) {
    let Some(value) = value else {
        return;
    };

    for mbid in split_musicbrainz_ids(value) {
        ids.entry(entity_type).or_default().insert(mbid);
    }
}

fn split_musicbrainz_ids(value: &str) -> Vec<String> {
    value
        .split(|character: char| {
            character.is_ascii_whitespace()
                || character == ','
                || character == ';'
                || character == '|'
                || character == '/'
        })
        .filter_map(|part| {
            let part = part
                .trim()
                .trim_matches('{')
                .trim_matches('}')
                .to_ascii_lowercase();
            is_musicbrainz_id(&part).then_some(part)
        })
        .collect()
}

fn is_musicbrainz_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }

    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }

    true
}

fn musicbrainz_entity_url(
    base_url: &str,
    entity_type: MusicBrainzEntityType,
    mbid: &str,
) -> String {
    format!(
        "{}/{}/{}",
        base_url.trim_end_matches('/'),
        entity_type.path(),
        mbid
    )
}

fn cover_art_archive_url(
    base_url: &str,
    entity_type: CoverArtArchiveEntityType,
    mbid: &str,
) -> String {
    format!(
        "{}/{}/{}",
        base_url.trim_end_matches('/'),
        entity_type.path(),
        mbid
    )
}

fn normalize_musicbrainz_document(
    target: &MusicBrainzLookupTarget,
    value: &Value,
) -> Result<MusicBrainzDocument, String> {
    let id = string_field(value, "id").unwrap_or_else(|| target.mbid.clone());
    if id != target.mbid {
        return Err(format!(
            "MusicBrainz returned {} but {} was requested",
            id, target.mbid
        ));
    }

    Ok(match target.entity_type {
        MusicBrainzEntityType::Recording => MusicBrainzDocument::Recording(MusicBrainzRecording {
            id,
            title: string_field(value, "title"),
            disambiguation: string_field(value, "disambiguation"),
            length_ms: value.get("length").and_then(Value::as_u64),
            first_release_date: string_field(value, "first-release-date"),
            artist_credit: artist_credit(value),
            isrcs: string_array_field(value, "isrcs"),
            releases: linked_releases(value.get("releases")),
        }),
        MusicBrainzEntityType::Track => MusicBrainzDocument::Track(MusicBrainzTrack {
            id,
            title: string_field(value, "title"),
            number: string_field(value, "number"),
            length_ms: value.get("length").and_then(Value::as_u64),
            artist_credit: artist_credit(value),
            recording: value.get("recording").and_then(linked_recording),
        }),
        MusicBrainzEntityType::Release => MusicBrainzDocument::Release(MusicBrainzRelease {
            id,
            title: string_field(value, "title"),
            disambiguation: string_field(value, "disambiguation"),
            date: string_field(value, "date"),
            country: string_field(value, "country"),
            status: string_field(value, "status"),
            barcode: string_field(value, "barcode"),
            artist_credit: artist_credit(value),
            release_group: value.get("release-group").and_then(linked_release_group),
            media: media(value.get("media")),
        }),
        MusicBrainzEntityType::ReleaseGroup => {
            MusicBrainzDocument::ReleaseGroup(MusicBrainzReleaseGroup {
                id,
                title: string_field(value, "title"),
                disambiguation: string_field(value, "disambiguation"),
                first_release_date: string_field(value, "first-release-date"),
                primary_type: string_field(value, "primary-type"),
                secondary_types: string_array_field(value, "secondary-types"),
                artist_credit: artist_credit(value),
                releases: linked_releases(value.get("releases")),
            })
        }
        MusicBrainzEntityType::Artist => MusicBrainzDocument::Artist(MusicBrainzArtist {
            id,
            name: string_field(value, "name"),
            sort_name: string_field(value, "sort-name"),
            disambiguation: string_field(value, "disambiguation"),
            artist_type: string_field(value, "type"),
            country: string_field(value, "country"),
        }),
    })
}

fn recording_candidates(value: &Value) -> Vec<MusicBrainzRecordingCandidate> {
    value
        .get("recordings")
        .and_then(Value::as_array)
        .map(|recordings| {
            recordings
                .iter()
                .filter_map(|recording| {
                    Some(MusicBrainzRecordingCandidate {
                        id: string_field(recording, "id")?,
                        score: recording.get("score").and_then(Value::as_u64),
                        title: string_field(recording, "title"),
                        disambiguation: string_field(recording, "disambiguation"),
                        length_ms: recording.get("length").and_then(Value::as_u64),
                        first_release_date: string_field(recording, "first-release-date"),
                        artist_credit: artist_credit(recording),
                        isrcs: string_array_field(recording, "isrcs"),
                        releases: linked_releases(recording.get("releases")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn release_candidates(value: &Value) -> Vec<MusicBrainzReleaseCandidate> {
    value
        .get("releases")
        .and_then(Value::as_array)
        .map(|releases| {
            releases
                .iter()
                .filter_map(|release| {
                    Some(MusicBrainzReleaseCandidate {
                        id: string_field(release, "id")?,
                        score: release.get("score").and_then(Value::as_u64),
                        title: string_field(release, "title"),
                        disambiguation: string_field(release, "disambiguation"),
                        date: string_field(release, "date"),
                        country: string_field(release, "country"),
                        status: string_field(release, "status"),
                        barcode: string_field(release, "barcode"),
                        artist_credit: artist_credit(release),
                        release_group: release.get("release-group").and_then(linked_release_group),
                        media: media(release.get("media")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cover_art_archive_candidates(
    target: &CoverArtArchiveTarget,
    value: &Value,
) -> Vec<CoverArtArchiveCandidate> {
    value
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .filter_map(|image| {
                    let image_url = string_field(image, "image")?;
                    Some(CoverArtArchiveCandidate {
                        id: format!(
                            "{}:{}:{}",
                            target.entity_type.path(),
                            target.mbid,
                            image_id(image).unwrap_or_else(|| image_url.clone())
                        ),
                        entity_type: target.entity_type,
                        mbid: target.mbid.clone(),
                        image_url,
                        thumbnail_url: thumbnail_url(image),
                        front: image
                            .get("front")
                            .and_then(Value::as_bool)
                            .unwrap_or_default(),
                        back: image
                            .get("back")
                            .and_then(Value::as_bool)
                            .unwrap_or_default(),
                        approved: image
                            .get("approved")
                            .and_then(Value::as_bool)
                            .unwrap_or_default(),
                        comment: string_field(image, "comment"),
                        edit_url: string_field(image, "edit"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn image_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .or_else(|| value.as_u64().map(|value| value.to_string()))
        })
        .filter(|value| !value.trim().is_empty())
}

fn thumbnail_url(value: &Value) -> Option<String> {
    let thumbnails = value.get("thumbnails")?;
    for key in ["500", "large", "250", "small", "1200"] {
        if let Some(url) = string_field(thumbnails, key) {
            return Some(url);
        }
    }
    None
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn string_array_field(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The full credited artist string, honoring MusicBrainz join phrases
/// (e.g. `"A & B"`). `None` when there is no artist credit.
fn artist_credit_string(value: &Value) -> Option<String> {
    let credits = value.get("artist-credit").and_then(Value::as_array)?;
    let mut out = String::new();
    for credit in credits {
        if let Some(name) = string_field(credit, "name").or_else(|| {
            credit
                .get("artist")
                .and_then(|artist| string_field(artist, "name"))
        }) {
            out.push_str(&name);
        }
        if let Some(join) = string_field(credit, "joinphrase") {
            out.push_str(&join);
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// The MBID of the first credited artist, if present.
fn first_artist_mbid(value: &Value) -> Option<String> {
    value
        .get("artist-credit")
        .and_then(Value::as_array)?
        .first()
        .and_then(|credit| credit.get("artist"))
        .and_then(|artist| string_field(artist, "id"))
}

/// The four-digit year from a MusicBrainz partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`).
fn year_from_date(date: &str) -> Option<i64> {
    date.get(0..4)
        .and_then(|year| year.parse::<i64>().ok())
        .filter(|year| *year > 0)
}

/// Title + artist (+ year) from a MusicBrainz recording document.
fn parse_recording_enrichment(value: &Value) -> MusicBrainzEnrichment {
    let recording_date = string_field(value, "first-release-date");
    let year = recording_date.as_deref().and_then(year_from_date);
    MusicBrainzEnrichment {
        title: string_field(value, "title"),
        artist_name: artist_credit_string(value),
        artist_mbid: first_artist_mbid(value),
        recording_date,
        year,
        ..Default::default()
    }
}

/// Fold a MusicBrainz release document into the enrichment: album title/artist, release
/// group, and the track's position (track/disc number, track total) located by matching
/// `recording_mbid` within the release's media tracklist. Falls back to the release date
/// for the year when the recording carried none.
fn merge_release_enrichment(
    enrichment: &mut MusicBrainzEnrichment,
    value: &Value,
    recording_mbid: &str,
) {
    enrichment.album_title = string_field(value, "title");
    enrichment.album_artist_name = artist_credit_string(value);
    enrichment.release_group_mbid = value
        .get("release-group")
        .and_then(|group| string_field(group, "id"));
    let release_year = string_field(value, "date")
        .as_deref()
        .and_then(year_from_date);

    if let Some(media) = value.get("media").and_then(Value::as_array) {
        for medium in media {
            let disc_number = medium.get("position").and_then(Value::as_i64);
            let track_total = medium.get("track-count").and_then(Value::as_i64);
            let Some(tracks) = medium.get("tracks").and_then(Value::as_array) else {
                continue;
            };
            for track in tracks {
                let recording_id = track
                    .get("recording")
                    .and_then(|recording| string_field(recording, "id"));
                if recording_id.as_deref() == Some(recording_mbid) {
                    enrichment.track_number = track
                        .get("position")
                        .and_then(Value::as_i64)
                        .or_else(|| string_field(track, "number").and_then(|n| n.parse().ok()));
                    enrichment.disc_number = disc_number;
                    enrichment.track_total = track_total;
                    if enrichment.year.is_none() {
                        enrichment.year = release_year;
                    }
                    return;
                }
            }
        }
    }
    if enrichment.year.is_none() {
        enrichment.year = release_year;
    }
}

fn artist_credit(value: &Value) -> Vec<String> {
    value
        .get("artist-credit")
        .and_then(Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|artist| {
                    string_field(artist, "name").or_else(|| {
                        artist
                            .get("artist")
                            .and_then(|artist| string_field(artist, "name"))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn linked_recording(value: &Value) -> Option<MusicBrainzLinkedRecording> {
    Some(MusicBrainzLinkedRecording {
        id: string_field(value, "id")?,
        title: string_field(value, "title"),
    })
}

fn linked_releases(value: Option<&Value>) -> Vec<MusicBrainzLinkedRelease> {
    value
        .and_then(Value::as_array)
        .map(|releases| {
            releases
                .iter()
                .filter_map(|release| {
                    Some(MusicBrainzLinkedRelease {
                        id: string_field(release, "id")?,
                        title: string_field(release, "title"),
                        date: string_field(release, "date"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn linked_release_group(value: &Value) -> Option<MusicBrainzLinkedReleaseGroup> {
    Some(MusicBrainzLinkedReleaseGroup {
        id: string_field(value, "id")?,
        title: string_field(value, "title"),
        primary_type: string_field(value, "primary-type"),
    })
}

fn media(value: Option<&Value>) -> Vec<MusicBrainzMedium> {
    value
        .and_then(Value::as_array)
        .map(|media| {
            media
                .iter()
                .map(|medium| MusicBrainzMedium {
                    position: medium.get("position").and_then(Value::as_u64),
                    format: string_field(medium, "format"),
                    track_count: medium.get("track-count").and_then(Value::as_u64),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicata_core::{
        Album, MetadataApprovalState, ProviderMapping, Track, TrackMetadataObservation,
    };
    use std::path::PathBuf;

    const RECORDING_ID: &str = "e3e2ace1-1312-4f76-94b8-e6c7d969b730";
    const TRACK_ID: &str = "0fbc8678-c5a5-3f7b-a46a-ac92f61f6bed";
    const RELEASE_ID: &str = "d08ef3f3-7c5d-4a1f-a28d-d81bead9e165";
    const RELEASE_GROUP_ID: &str = "9143b540-3d9f-33b9-9f19-57d10a016232";
    const ARTIST_ID: &str = "65f4f0c5-ef9e-490c-aee3-909e7ae6b2ab";

    #[test]
    fn extracts_deduplicated_lookup_targets_from_existing_mbids() {
        let track = fixture_track();
        let targets = musicbrainz_lookup_targets(&track, "https://musicbrainz.test/ws/2");

        assert_eq!(targets.len(), 5);
        assert!(targets.iter().any(|target| {
            target.entity_type == MusicBrainzEntityType::Recording
                && target.mbid == RECORDING_ID
                && target.lookup_url
                    == format!("https://musicbrainz.test/ws/2/recording/{RECORDING_ID}")
        }));
        assert!(targets.iter().any(|target| {
            target.entity_type == MusicBrainzEntityType::Track && target.mbid == TRACK_ID
        }));
        assert!(targets.iter().any(|target| {
            target.entity_type == MusicBrainzEntityType::Release && target.mbid == RELEASE_ID
        }));
        assert!(targets.iter().any(|target| {
            target.entity_type == MusicBrainzEntityType::ReleaseGroup
                && target.mbid == RELEASE_GROUP_ID
        }));
        assert!(targets.iter().any(|target| {
            target.entity_type == MusicBrainzEntityType::Artist && target.mbid == ARTIST_ID
        }));
    }

    #[test]
    fn normalizes_recording_lookup_json() {
        let target = MusicBrainzLookupTarget {
            entity_type: MusicBrainzEntityType::Recording,
            mbid: RECORDING_ID.to_string(),
            lookup_url: format!("https://musicbrainz.test/ws/2/recording/{RECORDING_ID}"),
        };
        let value = serde_json::json!({
            "id": RECORDING_ID,
            "title": "Brzi Vavilon",
            "length": 245000,
            "first-release-date": "1994",
            "artist-credit": [
                { "name": "Darkwood Dub", "artist": { "id": ARTIST_ID, "name": "Darkwood Dub" } }
            ],
            "isrcs": ["USRC17607839"],
            "releases": [
                { "id": RELEASE_ID, "title": "Paramparcad", "date": "1994" }
            ]
        });

        let document = normalize_musicbrainz_document(&target, &value).expect("document");
        let MusicBrainzDocument::Recording(recording) = document else {
            panic!("expected recording");
        };

        assert_eq!(recording.id, RECORDING_ID);
        assert_eq!(recording.title.as_deref(), Some("Brzi Vavilon"));
        assert_eq!(recording.artist_credit, vec!["Darkwood Dub"]);
        assert_eq!(recording.isrcs, vec!["USRC17607839"]);
        assert_eq!(recording.releases[0].title.as_deref(), Some("Paramparcad"));
    }

    #[test]
    fn builds_track_and_album_candidate_queries() {
        let mut track = fixture_track();
        track.observed_metadata.clear();
        let album = fixture_album();

        assert_eq!(
            musicbrainz_track_candidate_query(&track),
            "recording:\"Brzi Vavilon\" AND artist:\"Darkwood Dub\" AND release:\"Paramparcad\" AND date:1994"
        );
        assert_eq!(
            musicbrainz_album_candidate_query(&album),
            "release:\"Paramparcad\" AND artist:\"Darkwood Dub\" AND date:1994"
        );
        assert_eq!(
            musicbrainz_search_url(
                "https://musicbrainz.test/ws/2",
                MusicBrainzSearchEntityType::Recording,
                &musicbrainz_track_candidate_query(&track),
                5,
            ),
            "https://musicbrainz.test/ws/2/recording?query=recording%3A%22Brzi%20Vavilon%22%20AND%20artist%3A%22Darkwood%20Dub%22%20AND%20release%3A%22Paramparcad%22%20AND%20date%3A1994&fmt=json&limit=5"
        );
    }

    #[test]
    fn skips_candidate_search_when_existing_mbids_identify_track_or_album() {
        let client = MusicBrainzClient::new("https://musicbrainz.test/ws/2");
        let track = fixture_track();
        let album = fixture_album();

        let track_search = client.search_track_candidates(&track, 5);
        assert!(!track_search.searched);
        assert_eq!(
            track_search.skipped_reason.as_deref(),
            Some("track already has MusicBrainz identifiers")
        );

        let album_search = client.search_album_candidates(&album, &[track], 5);
        assert!(!album_search.searched);
        assert_eq!(
            album_search.skipped_reason.as_deref(),
            Some("album already has MusicBrainz release identifiers")
        );
    }

    #[test]
    fn normalizes_candidate_search_json() {
        let value = serde_json::json!({
            "recordings": [
                {
                    "id": RECORDING_ID,
                    "score": 98,
                    "title": "Brzi Vavilon",
                    "length": 245000,
                    "artist-credit": [{ "name": "Darkwood Dub" }],
                    "releases": [{ "id": RELEASE_ID, "title": "Paramparcad", "date": "1994" }]
                }
            ],
            "releases": [
                {
                    "id": RELEASE_ID,
                    "score": 96,
                    "title": "Paramparcad",
                    "date": "1994",
                    "artist-credit": [{ "name": "Darkwood Dub" }],
                    "release-group": {
                        "id": RELEASE_GROUP_ID,
                        "title": "Paramparcad",
                        "primary-type": "Album"
                    },
                    "media": [{ "position": 1, "format": "CD", "track-count": 9 }]
                }
            ]
        });

        let recordings = recording_candidates(&value);
        assert_eq!(recordings.len(), 1);
        assert_eq!(recordings[0].score, Some(98));
        assert_eq!(recordings[0].artist_credit, vec!["Darkwood Dub"]);

        let releases = release_candidates(&value);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].score, Some(96));
        assert_eq!(releases[0].media[0].track_count, Some(9));
    }

    #[test]
    fn builds_and_normalizes_cover_art_archive_candidates() {
        let track = fixture_track();
        let targets =
            cover_art_archive_targets(std::slice::from_ref(&track), "https://coverart.test");

        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|target| {
            target.entity_type == CoverArtArchiveEntityType::Release
                && target.lookup_url == format!("https://coverart.test/release/{RELEASE_ID}")
        }));
        assert!(targets.iter().any(|target| {
            target.entity_type == CoverArtArchiveEntityType::ReleaseGroup
                && target.lookup_url
                    == format!("https://coverart.test/release-group/{RELEASE_GROUP_ID}")
        }));

        let value = serde_json::json!({
            "images": [
                {
                    "id": 123,
                    "image": "https://archive.test/full.jpg",
                    "front": true,
                    "back": false,
                    "approved": true,
                    "comment": "Front",
                    "edit": "https://musicbrainz.test/edit/1",
                    "thumbnails": {
                        "small": "https://archive.test/small.jpg",
                        "500": "https://archive.test/500.jpg"
                    }
                }
            ]
        });
        let candidates = cover_art_archive_candidates(&targets[0], &value);

        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].front);
        assert!(candidates[0].approved);
        assert_eq!(
            candidates[0].thumbnail_url.as_deref(),
            Some("https://archive.test/500.jpg")
        );
    }

    #[test]
    fn skips_cover_art_archive_without_release_ids() {
        let client = MusicBrainzClient::new("https://musicbrainz.test/ws/2");
        let album = fixture_album();
        let mut track = fixture_track();
        track.observed_metadata[0].musicbrainz_release_id = None;
        track.observed_metadata[0].musicbrainz_release_group_id = None;
        let response = client.cover_art_archive_candidates(&album, &[track], 5);

        assert!(!response.searched);
        assert_eq!(
            response.skipped_reason.as_deref(),
            Some("album has no MusicBrainz release or release-group identifiers")
        );
    }

    fn fixture_album() -> Album {
        Album {
            id: "album_1".to_string(),
            title: "Paramparcad".to_string(),
            artist_id: "artist_1".to_string(),
            artist_name: "Darkwood Dub".to_string(),
            year: Some(1994),
            track_count: 1,
            artwork_url: None,
            artwork_path: None,
        }
    }

    #[test]
    fn parses_recording_enrichment_with_join_phrase() {
        let value = serde_json::json!({
            "id": RECORDING_ID,
            "title": "Strobe",
            "first-release-date": "2010-10-05",
            "artist-credit": [
                { "name": "deadmau5", "joinphrase": " & ",
                  "artist": { "id": ARTIST_ID, "name": "deadmau5" } },
                { "name": "Kaskade",
                  "artist": { "id": "k-id", "name": "Kaskade" } }
            ]
        });
        let enrichment = parse_recording_enrichment(&value);
        assert_eq!(enrichment.title.as_deref(), Some("Strobe"));
        assert_eq!(enrichment.artist_name.as_deref(), Some("deadmau5 & Kaskade"));
        assert_eq!(enrichment.artist_mbid.as_deref(), Some(ARTIST_ID));
        assert_eq!(enrichment.year, Some(2010));
        assert_eq!(enrichment.recording_date.as_deref(), Some("2010-10-05"));
    }

    #[test]
    fn merges_release_enrichment_locates_track_in_tracklist() {
        let mut enrichment = parse_recording_enrichment(&serde_json::json!({
            "id": RECORDING_ID,
            "title": "Strobe",
            "artist-credit": [{ "name": "deadmau5",
                "artist": { "id": ARTIST_ID, "name": "deadmau5" } }]
        }));
        // No recording date → year should fall back to the release date.
        assert_eq!(enrichment.year, None);

        let release = serde_json::json!({
            "id": RELEASE_ID,
            "title": "For Lack of a Better Name",
            "date": "2009-09-22",
            "artist-credit": [{ "name": "deadmau5",
                "artist": { "id": ARTIST_ID, "name": "deadmau5" } }],
            "release-group": { "id": RELEASE_GROUP_ID, "title": "For Lack of a Better Name" },
            "media": [{
                "position": 1,
                "track-count": 9,
                "tracks": [
                    { "position": 1, "number": "1", "recording": { "id": "other-rec" } },
                    { "position": 7, "number": "7", "recording": { "id": RECORDING_ID } }
                ]
            }]
        });
        merge_release_enrichment(&mut enrichment, &release, RECORDING_ID);

        assert_eq!(enrichment.album_title.as_deref(), Some("For Lack of a Better Name"));
        assert_eq!(enrichment.album_artist_name.as_deref(), Some("deadmau5"));
        assert_eq!(enrichment.release_group_mbid.as_deref(), Some(RELEASE_GROUP_ID));
        assert_eq!(enrichment.track_number, Some(7));
        assert_eq!(enrichment.track_total, Some(9));
        assert_eq!(enrichment.disc_number, Some(1));
        assert_eq!(enrichment.year, Some(2009));
    }

    #[test]
    #[ignore = "live network; run with: cargo test -- --ignored live_musicbrainz_enrichment"]
    fn live_musicbrainz_enrichment() {
        // A real, stable recording + release on MusicBrainz (deadmau5 — "Strobe").
        let recording = "772bf141-684a-4c6a-96d9-3a004ae1676e";
        let release = "313ffe72-327e-4811-9bd0-ef6a1e1cefd0";
        let enrichment = MusicBrainzClient::default()
            .fetch_enrichment(recording, Some(release))
            .expect("fetch");
        eprintln!("{enrichment:#?}");
        assert_eq!(enrichment.title.as_deref(), Some("Strobe"));
        assert!(enrichment.artist_name.is_some());
        assert!(enrichment.album_title.is_some());
        assert!(enrichment.track_number.is_some());
    }

    fn fixture_track() -> Track {
        Track {
            id: "track_1".to_string(),
            provider: ProviderMapping {
                provider_id: "local-disk".to_string(),
                item_id: "album/song.mp3".to_string(),
            },
            observed_metadata: vec![TrackMetadataObservation {
                source: "embedded_tag".to_string(),
                confidence: 0.95,
                observed_at_unix_seconds: 1_800_000_000,
                approval_state: MetadataApprovalState::Observed,
                field_observations: Vec::new(),
                title: Some("Brzi Vavilon".to_string()),
                artist_name: Some("Darkwood Dub".to_string()),
                album_artist_name: None,
                album_title: Some("Paramparcad".to_string()),
                recording_date: None,
                year: Some(1994),
                track_number: Some(1),
                track_total: None,
                disc_number: None,
                disc_total: None,
                genres: Vec::new(),
                composers: Vec::new(),
                lyrics: None,
                musicbrainz_recording_id: Some(RECORDING_ID.to_string()),
                musicbrainz_track_id: Some(TRACK_ID.to_string()),
                musicbrainz_release_id: Some(RELEASE_ID.to_string()),
                musicbrainz_release_group_id: Some(RELEASE_GROUP_ID.to_string()),
                musicbrainz_artist_id: Some(format!("{ARTIST_ID}; {ARTIST_ID}; invalid")),
                musicbrainz_release_artist_id: None,
                isrc: None,
                embedded_artwork_count: 0,
            }],
            title: "Brzi Vavilon".to_string(),
            artist_id: "artist_1".to_string(),
            artist_name: "Darkwood Dub".to_string(),
            album_id: "album_1".to_string(),
            album_title: "Paramparcad".to_string(),
            year: Some(1994),
            track_number: Some(1),
            disc_number: None,
            extension: "mp3".to_string(),
            file_size_bytes: None,
            duration_seconds: None,
            modified_at_unix_seconds: None,
            content_hash: None,
            relative_path: "album/song.mp3".to_string(),
            stream_url: "/api/tracks/track_1/stream".to_string(),
            added_at_unix_seconds: None,
            path: PathBuf::from("/music/album/song.mp3"),
        }
    }
}
