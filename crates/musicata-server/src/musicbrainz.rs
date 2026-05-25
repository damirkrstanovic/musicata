use musicata_core::Track;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DEFAULT_MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org/ws/2";
const MUSICBRAINZ_TIMEOUT: Duration = Duration::from_secs(10);
const MUSICBRAINZ_REQUEST_INTERVAL: Duration = Duration::from_millis(1100);

#[derive(Clone)]
pub struct MusicBrainzClient {
    base_url: String,
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
}

fn musicbrainz_request_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("MusicBrainz returned HTTP {status}"),
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
    use musicata_core::{MetadataApprovalState, ProviderMapping, Track, TrackMetadataObservation};
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
            extension: "mp3".to_string(),
            file_size_bytes: None,
            modified_at_unix_seconds: None,
            content_hash: None,
            relative_path: "album/song.mp3".to_string(),
            stream_url: "/api/tracks/track_1/stream".to_string(),
            path: PathBuf::from("/music/album/song.mp3"),
        }
    }
}
