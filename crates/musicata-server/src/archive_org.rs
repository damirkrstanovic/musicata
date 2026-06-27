//! Internet Archive source: a browse-only ([`ProviderCapabilities::STREAM_ONLY`]) provider
//! over the audio files of one Archive.org item.
//!
//! Like the podcast and internet-radio sources, an Archive.org item never merges into the
//! scanned library — its files are streamed, not scanned. The item **identifier** is stored
//! in the source's `host` column (a bare id, or an `archive.org/details/<id>` URL we extract
//! it from); `browse` fetches the item's JSON metadata
//! (`https://archive.org/metadata/<id>`) and lists its audio files as episodes (download
//! URLs carried inline), and `resolve` maps a file id back to its stream. No API key — the
//! Archive's metadata and download endpoints are public. See `docs/decisions.md`.

use std::time::Duration;

use musicata_core::{BrowseEntry, ProviderCapabilities, StreamSpec};
use musicata_storage::SourceRecord;
use serde::Deserialize;

/// How long to wait on archive.org before giving up.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Bootstrap config: the Archive.org item identifier.
pub struct ArchiveConfig {
    pub identifier: String,
}

impl ArchiveConfig {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let raw = record
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Internet Archive source needs an item identifier"))?;
        let identifier = extract_identifier(raw);
        if identifier.is_empty() {
            anyhow::bail!("could not read an Internet Archive identifier from {raw:?}");
        }
        Ok(Self { identifier })
    }
}

/// Accept a bare identifier or an `archive.org/{details,download,metadata}/<id>` URL and
/// pull out the identifier.
fn extract_identifier(raw: &str) -> String {
    let no_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let trimmed = no_scheme.trim_end_matches('/');
    for marker in ["details/", "download/", "metadata/"] {
        if let Some(pos) = trimmed.find(marker) {
            let after = &trimmed[pos + marker.len()..];
            let id = after.split('/').next().unwrap_or(after);
            if !id.is_empty() {
                return id.to_string();
            }
        }
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Stable provider id for an Archive.org item, e.g. `archive:gd1977-05-08`.
pub fn archive_provider_id(identifier: &str) -> String {
    format!("archive:{}", identifier.to_lowercase())
}

// ---- Parsing (pure, unit-tested) ----

/// The parsed view of an Archive.org item.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveItem {
    pub title: Option<String>,
    pub tracks: Vec<ArchiveTrack>,
}

/// One playable audio file.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveTrack {
    /// Id = the file name within the item (also used to build the download URL).
    pub id: String,
    pub title: String,
    pub stream_url: String,
}

#[derive(Debug, Deserialize)]
struct MetadataDoc {
    #[serde(default)]
    metadata: ItemMeta,
    #[serde(default)]
    files: Vec<RawFile>,
}

#[derive(Debug, Default, Deserialize)]
struct ItemMeta {
    /// IA metadata fields are sometimes a string, sometimes an array; keep it raw.
    #[serde(default)]
    title: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// An IA metadata value can be a JSON string or a one-element array of strings; coerce.
fn coerce_string(value: Option<serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .find_map(|item| item.as_str().map(str::to_string)),
        _ => None,
    }
}

/// Rank an audio format so we keep one file per track (IA carries the same track in
/// several formats). Higher is better; `None` means "not audio".
fn audio_rank(file: &RawFile) -> Option<u8> {
    let format = file.format.as_deref().unwrap_or("").to_ascii_lowercase();
    let name = file.name.to_ascii_lowercase();
    let has = |needle: &str| format.contains(needle) || name.ends_with(needle);
    if has("flac") {
        Some(5)
    } else if has(".mp3") || format.contains("mp3") {
        Some(4)
    } else if has(".opus") || format.contains("opus") {
        Some(3)
    } else if has(".ogg") || format.contains("ogg") || format.contains("vorbis") {
        Some(2)
    } else if has(".m4a") || has(".aac") || format.contains("aac") || has(".wav") {
        Some(1)
    } else {
        None
    }
}

/// The stem of a file name (without its extension), used to group the same track's
/// multiple formats together.
fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((base, _ext)) => base,
        None => name,
    }
}

/// Percent-encode a download path so spaces and other characters survive in the URL.
/// `/` is kept as a path separator; the RFC 3986 unreserved set passes through.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse an Archive.org item metadata document into an [`ArchiveItem`]. Non-audio files
/// are dropped; when a track exists in several formats, the best one is kept (one entry
/// per track).
pub fn parse_item(json: &str, identifier: &str) -> anyhow::Result<ArchiveItem> {
    let doc: MetadataDoc = serde_json::from_str(json)
        .map_err(|error| anyhow::anyhow!("parse Internet Archive metadata: {error}"))?;

    // Keep the best-ranked audio file per stem.
    let mut best: Vec<(String, u8, RawFile)> = Vec::new();
    for file in doc.files {
        let Some(rank) = audio_rank(&file) else {
            continue;
        };
        let key = stem(&file.name).to_string();
        match best.iter_mut().find(|(existing, _, _)| *existing == key) {
            Some(slot) if slot.1 >= rank => {}
            Some(slot) => *slot = (key, rank, file),
            None => best.push((key, rank, file)),
        }
    }

    let tracks = best
        .into_iter()
        .map(|(_, _, file)| {
            let title = file
                .title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| stem(&file.name).to_string());
            let stream_url = format!(
                "https://archive.org/download/{}/{}",
                encode_path(identifier),
                encode_path(&file.name)
            );
            ArchiveTrack {
                id: file.name,
                title,
                stream_url,
            }
        })
        .collect();

    Ok(ArchiveItem {
        title: coerce_string(doc.metadata.title).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
        tracks,
    })
}

// ---- Provider ----

/// One Archive.org item surfaced as a browse-only source.
pub struct ArchiveProvider {
    provider_id: String,
    identifier: String,
}

impl ArchiveProvider {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let config = ArchiveConfig::from_record(record)?;
        Ok(Self {
            provider_id: archive_provider_id(&config.identifier),
            identifier: config.identifier,
        })
    }

    pub fn provider_id(&self) -> &String {
        &self.provider_id
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::STREAM_ONLY
    }

    async fn fetch_item(&self) -> anyhow::Result<ArchiveItem> {
        let identifier = self.identifier.clone();
        tokio::task::spawn_blocking(move || {
            let user_agent = format!("Musicata/{}", env!("CARGO_PKG_VERSION"));
            let agent = ureq::AgentBuilder::new()
                .user_agent(&user_agent)
                .timeout(TIMEOUT)
                .build();
            let url = format!("https://archive.org/metadata/{}", encode_path(&identifier));
            let body = agent
                .get(&url)
                .call()
                .map_err(|error| anyhow::anyhow!("fetch Internet Archive metadata: {error}"))?
                .into_string()
                .map_err(|error| anyhow::anyhow!("read Internet Archive metadata: {error}"))?;
            parse_item(&body, &identifier)
        })
        .await?
    }

    pub async fn validate(&self) -> anyhow::Result<()> {
        let item = self.fetch_item().await?;
        if item.tracks.is_empty() {
            anyhow::bail!("Internet Archive item has no playable audio files");
        }
        Ok(())
    }

    pub async fn browse(&self) -> anyhow::Result<Vec<BrowseEntry>> {
        let item = self.fetch_item().await?;
        let album = item.title;
        Ok(item
            .tracks
            .into_iter()
            .map(|track| BrowseEntry {
                id: track.id,
                title: track.title,
                subtitle: album.clone(),
                stream_url: Some(track.stream_url),
                homepage_url: None,
                is_container: false,
            })
            .collect())
    }

    pub async fn resolve(&self, item_id: &str) -> anyhow::Result<StreamSpec> {
        let item = self.fetch_item().await?;
        let track = item
            .tracks
            .into_iter()
            .find(|track| track.id == item_id)
            .ok_or_else(|| anyhow::anyhow!("unknown Internet Archive file: {item_id}"))?;
        Ok(StreamSpec {
            url: track.stream_url,
            title: Some(track.title),
            content_type: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"{
        "metadata": { "identifier": "test-item", "title": "Live At The Example" },
        "files": [
            { "name": "gd77-d1t01.flac", "title": "Opener", "format": "Flac" },
            { "name": "gd77-d1t01.mp3", "title": "Opener", "format": "VBR MP3" },
            { "name": "gd77-d1t02.mp3", "title": "Second", "format": "VBR MP3" },
            { "name": "cover art.jpg", "format": "JPEG" },
            { "name": "gd77.txt", "format": "Text" }
        ]
    }"#;

    #[test]
    fn parses_audio_files_dedup_by_format() {
        let item = parse_item(META, "test-item").expect("parse");
        assert_eq!(item.title.as_deref(), Some("Live At The Example"));
        // The image + text are dropped; the FLAC wins over the MP3 of the same track,
        // so two tracks remain.
        assert_eq!(item.tracks.len(), 2);

        let opener = item.tracks.iter().find(|t| t.title == "Opener").expect("opener");
        assert_eq!(opener.id, "gd77-d1t01.flac"); // FLAC preferred over MP3
        assert_eq!(
            opener.stream_url,
            "https://archive.org/download/test-item/gd77-d1t01.flac"
        );
    }

    #[test]
    fn title_can_be_an_array() {
        let json = r#"{ "metadata": { "title": ["First Title", "Alt"] },
            "files": [{ "name": "a.mp3", "format": "VBR MP3" }] }"#;
        let item = parse_item(json, "x").expect("parse");
        assert_eq!(item.title.as_deref(), Some("First Title"));
    }

    #[test]
    fn extract_identifier_handles_urls_and_bare_ids() {
        assert_eq!(extract_identifier("gd1977-05-08"), "gd1977-05-08");
        assert_eq!(
            extract_identifier("https://archive.org/details/gd1977-05-08"),
            "gd1977-05-08"
        );
        assert_eq!(
            extract_identifier("archive.org/download/gd1977-05-08/file.mp3"),
            "gd1977-05-08"
        );
    }

    #[test]
    fn encode_path_escapes_spaces_keeps_separators() {
        assert_eq!(encode_path("a b/c.mp3"), "a%20b/c.mp3");
    }

    #[test]
    fn provider_id_is_namespaced() {
        assert_eq!(archive_provider_id("GD1977"), "archive:gd1977");
    }

    /// Live browse against a real item. Ignored by default (network); run with `--ignored`
    /// and `MUSICATA_ARCHIVE_ITEM=<identifier>`.
    #[tokio::test]
    #[ignore = "hits the network; set MUSICATA_ARCHIVE_ITEM"]
    async fn archive_live_browse() {
        let identifier =
            std::env::var("MUSICATA_ARCHIVE_ITEM").expect("set MUSICATA_ARCHIVE_ITEM");
        let record = SourceRecord {
            id: String::new(),
            kind: "archive".into(),
            display_name: "Live".into(),
            enabled: true,
            host: Some(identifier),
            share: None,
            base_path: None,
            username: None,
            password: None,
            domain: None,
            created_at_unix_seconds: 0,
        };
        let provider = ArchiveProvider::from_record(&record).expect("build provider");
        let entries = provider.browse().await.expect("browse live item");
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|entry| entry.stream_url.is_some()));
    }
}
