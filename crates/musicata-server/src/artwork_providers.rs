//! Pluggable artwork providers — fetch a missing album cover from an external API.
//!
//! Each provider is a small synchronous `ureq` client (like [`crate::musicbrainz`]);
//! the background fill pass calls them inside `spawn_blocking`. The registry tries
//! providers in priority order — exact MusicBrainz-id matches first (Cover Art Archive,
//! later fanart.tv), then text search (iTunes, Deezer) — and skips id-only providers
//! when the album has no MusicBrainz ids. Each provider self-throttles (a shared
//! last-request clock) so concurrent fill workers can't exceed an API's rate limit.
//!
//! Parsing is factored into pure functions so it's unit-tested with canned JSON; the
//! HTTP base URLs are injectable so tests can point at a local server.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use serde_json::Value;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Identify an image format from its magic bytes, returning the file extension. `None`
/// for anything that isn't a recognized image (e.g. an HTML/JSON error page).
fn image_extension_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Download a cover image, returning its bytes and a file extension derived from the
/// actual content (magic bytes). Bounded so a mis-sized response can't exhaust memory;
/// a non-image body (an HTML/JSON error page) is rejected rather than cached as `.jpg`.
pub fn download_image(url: &str) -> Result<(Vec<u8>, &'static str), String> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let response = agent().get(url).call().map_err(http_error)?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.is_empty() {
        return Err("empty image response".to_string());
    }
    let ext = image_extension_from_magic(&bytes)
        .ok_or_else(|| "response is not a recognized image".to_string())?;
    Ok((bytes, ext))
}

/// Stable artwork-cache key for an album's acquired cover — derived from the album id
/// and source URL, so a changed cover yields a new key (busting the client cache).
pub fn acquired_cache_key(album_id: &str, image_url: &str) -> String {
    let digest = Md5::digest(format!("{album_id}|{image_url}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("acq{hex}")
}

/// What we know about an album when looking for its cover.
#[derive(Clone, Debug)]
pub struct ArtworkQuery {
    pub artist: String,
    pub album: String,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
}

impl ArtworkQuery {
    fn has_mbid(&self) -> bool {
        self.release_mbid.is_some() || self.release_group_mbid.is_some()
    }
}

/// What we know about an artist when looking for their image. Name-based: this library
/// (like most) carries no artist MusicBrainz ids, so id-keyed providers can't help.
#[derive(Clone, Debug)]
pub struct ArtistArtworkQuery {
    pub name: String,
}

/// A found cover: where to download it and (best-effort) its width.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtworkResult {
    pub provider: &'static str,
    pub image_url: String,
    pub width: Option<u32>,
}

/// One artwork source. Synchronous (ureq); called from `spawn_blocking`.
pub trait ArtworkProvider: Send + Sync {
    fn id(&self) -> &'static str;
    /// Needs MusicBrainz ids — skipped when the album has none.
    fn needs_mbid(&self) -> bool;
    /// Search for the album's cover. `Ok(None)` = no confident match; `Err` = a
    /// transport/HTTP failure (logged, then the next provider is tried).
    fn find_album_cover(&self, query: &ArtworkQuery) -> Result<Option<ArtworkResult>, String>;

    /// Search for an artist image by name. Defaults to `Ok(None)` — only providers with
    /// a name-based artist endpoint (Deezer) implement it.
    fn find_artist_image(
        &self,
        _query: &ArtistArtworkQuery,
    ) -> Result<Option<ArtworkResult>, String> {
        Ok(None)
    }
}

/// Whether a candidate artist name loosely matches the query (one contains the other,
/// after normalization), guarding name search against grabbing the wrong artist.
fn artist_matches(query_name: &str, cand_name: &str) -> bool {
    let (a, b) = (normalize(query_name), normalize(cand_name));
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// A shared per-provider request clock: serializes calls and spaces them by `interval`
/// so concurrent fill workers stay within an API's rate limit.
struct RateLimiter {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateLimiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// Run `f` no sooner than `interval` after the previous call. Holding the lock
    /// across `f` serializes this provider's requests.
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let mut last = self.last.lock().expect("rate limiter poisoned");
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.interval {
                std::thread::sleep(self.interval - elapsed);
            }
        }
        let result = f();
        *last = Some(Instant::now());
        result
    }
}

fn agent() -> ureq::Agent {
    let user_agent = format!(
        "Musicata/{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    );
    ureq::AgentBuilder::new()
        .user_agent(&user_agent)
        .timeout(DEFAULT_TIMEOUT)
        .build()
}

fn http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("HTTP {status}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}

/// Loose normalization for matching titles/artists across providers: lowercase, keep
/// alphanumerics + spaces, collapse whitespace.
fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = true;
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Whether a candidate's artist/album loosely match the query (one contains the other),
/// guarding text search against grabbing the wrong cover.
fn matches(query_artist: &str, query_album: &str, cand_artist: &str, cand_album: &str) -> bool {
    let loose = |a: &str, b: &str| {
        let (a, b) = (normalize(a), normalize(b));
        !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
    };
    loose(query_artist, cand_artist) && loose(query_album, cand_album)
}

// ---- iTunes / Apple Search -------------------------------------------------

pub struct ItunesProvider {
    base_url: String,
    agent: ureq::Agent,
    limiter: RateLimiter,
}

impl ItunesProvider {
    pub fn new() -> Self {
        Self::with_base_url("https://itunes.apple.com")
    }
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            agent: agent(),
            // Apple's fair-use guidance is ~20 requests/minute.
            limiter: RateLimiter::new(Duration::from_secs(3)),
        }
    }
}

impl ArtworkProvider for ItunesProvider {
    fn id(&self) -> &'static str {
        "itunes"
    }
    fn needs_mbid(&self) -> bool {
        false
    }
    fn find_album_cover(&self, query: &ArtworkQuery) -> Result<Option<ArtworkResult>, String> {
        let term = format!("{} {}", query.artist, query.album);
        let url = format!("{}/search", self.base_url);
        let value = self.limiter.run(|| {
            self.agent
                .get(&url)
                .query("term", &term)
                .query("entity", "album")
                .query("media", "music")
                .query("limit", "8")
                .call()
                .map_err(http_error)?
                .into_json::<Value>()
                .map_err(|error| error.to_string())
        })?;
        Ok(parse_itunes(&value, query))
    }
}

/// Pick the best iTunes album result and upgrade its 100×100 thumbnail to 600×600.
fn parse_itunes(value: &Value, query: &ArtworkQuery) -> Option<ArtworkResult> {
    let results = value.get("results")?.as_array()?;
    let chosen = results.iter().find(|r| {
        matches(
            &query.artist,
            &query.album,
            r.get("artistName").and_then(Value::as_str).unwrap_or(""),
            r.get("collectionName")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
    })?;
    let art = chosen.get("artworkUrl100").and_then(Value::as_str)?;
    // Apple's thumbnail URLs usually embed the size; swap to a larger render when the
    // token is present, and only then claim the larger width. Otherwise keep the URL as-is
    // (artworkUrl100 is a 100px render by Apple's API contract).
    let (image_url, width) = if art.contains("100x100bb") {
        (art.replace("100x100bb", "600x600bb"), Some(600))
    } else {
        (art.to_string(), Some(100))
    };
    Some(ArtworkResult {
        provider: "itunes",
        image_url,
        width,
    })
}

// ---- Deezer ----------------------------------------------------------------

pub struct DeezerProvider {
    base_url: String,
    agent: ureq::Agent,
    limiter: RateLimiter,
}

impl DeezerProvider {
    pub fn new() -> Self {
        Self::with_base_url("https://api.deezer.com")
    }
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            agent: agent(),
            limiter: RateLimiter::new(Duration::from_millis(250)),
        }
    }
}

impl ArtworkProvider for DeezerProvider {
    fn id(&self) -> &'static str {
        "deezer"
    }
    fn needs_mbid(&self) -> bool {
        false
    }
    fn find_album_cover(&self, query: &ArtworkQuery) -> Result<Option<ArtworkResult>, String> {
        let q = format!("artist:\"{}\" album:\"{}\"", query.artist, query.album);
        let url = format!("{}/search/album", self.base_url);
        let value = self.limiter.run(|| {
            self.agent
                .get(&url)
                .query("q", &q)
                .call()
                .map_err(http_error)?
                .into_json::<Value>()
                .map_err(|error| error.to_string())
        })?;
        Ok(parse_deezer(&value, query))
    }

    fn find_artist_image(
        &self,
        query: &ArtistArtworkQuery,
    ) -> Result<Option<ArtworkResult>, String> {
        let url = format!("{}/search/artist", self.base_url);
        let value = self.limiter.run(|| {
            self.agent
                .get(&url)
                .query("q", &query.name)
                .call()
                .map_err(http_error)?
                .into_json::<Value>()
                .map_err(|error| error.to_string())
        })?;
        Ok(parse_deezer_artist(&value, query))
    }
}

fn parse_deezer(value: &Value, query: &ArtworkQuery) -> Option<ArtworkResult> {
    let data = value.get("data")?.as_array()?;
    let chosen = data.iter().find(|r| {
        matches(
            &query.artist,
            &query.album,
            r.get("artist")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            r.get("title").and_then(Value::as_str).unwrap_or(""),
        )
    })?;
    let cover = chosen
        .get("cover_xl")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())?;
    Some(ArtworkResult {
        provider: "deezer",
        image_url: cover.to_string(),
        width: Some(1000),
    })
}

/// Pick an artist image from a Deezer `/search/artist` response: the first candidate
/// whose name loosely matches, preferring the largest portrait. Deezer serves a generic
/// silhouette for artists it has no photo for; those URLs contain `/artist//` (an empty
/// id segment), so skip them.
fn parse_deezer_artist(value: &Value, query: &ArtistArtworkQuery) -> Option<ArtworkResult> {
    let data = value.get("data")?.as_array()?;
    let chosen = data.iter().find(|r| {
        artist_matches(
            &query.name,
            r.get("name").and_then(Value::as_str).unwrap_or(""),
        )
    })?;
    let picture = chosen
        .get("picture_xl")
        .or_else(|| chosen.get("picture_big"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && !url.contains("/artist//"))?;
    Some(ArtworkResult {
        provider: "deezer",
        image_url: picture.to_string(),
        width: Some(1000),
    })
}

// ---- Cover Art Archive (MusicBrainz-id keyed) ------------------------------

pub struct CoverArtArchiveProvider {
    base_url: String,
    agent: ureq::Agent,
    limiter: RateLimiter,
}

impl CoverArtArchiveProvider {
    pub fn new() -> Self {
        Self::with_base_url("https://coverartarchive.org")
    }
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            agent: agent(),
            limiter: RateLimiter::new(Duration::from_millis(1100)),
        }
    }

    fn fetch(&self, entity: &str, mbid: &str) -> Result<Option<ArtworkResult>, String> {
        let url = format!("{}/{entity}/{mbid}", self.base_url);
        let result = self.limiter.run(|| match self.agent.get(&url).call() {
            Ok(response) => response
                .into_json::<Value>()
                .map(Some)
                .map_err(|error| error.to_string()),
            // A release with no cover returns 404 — that's "no match", not an error.
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(error) => Err(http_error(error)),
        })?;
        Ok(result.as_ref().and_then(parse_caa))
    }
}

impl ArtworkProvider for CoverArtArchiveProvider {
    fn id(&self) -> &'static str {
        "coverartarchive"
    }
    fn needs_mbid(&self) -> bool {
        true
    }
    fn find_album_cover(&self, query: &ArtworkQuery) -> Result<Option<ArtworkResult>, String> {
        // Prefer the specific release; fall back to the release group.
        if let Some(mbid) = &query.release_mbid
            && let Some(found) = self.fetch("release", mbid)?
        {
            return Ok(Some(found));
        }
        if let Some(mbid) = &query.release_group_mbid
            && let Some(found) = self.fetch("release-group", mbid)?
        {
            return Ok(Some(found));
        }
        Ok(None)
    }
}

fn parse_caa(value: &Value) -> Option<ArtworkResult> {
    let images = value.get("images")?.as_array()?;
    let front = images
        .iter()
        .find(|image| image.get("front").and_then(Value::as_bool) == Some(true))
        .or_else(|| images.first())?;
    let image_url = front.get("image").and_then(Value::as_str)?;
    Some(ArtworkResult {
        provider: "coverartarchive",
        image_url: image_url.to_string(),
        width: None,
    })
}

// ---- fanart.tv (release-group MBID keyed, needs an API key) -----------------

pub struct FanartTvProvider {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
    limiter: RateLimiter,
}

impl FanartTvProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://webservice.fanart.tv", api_key)
    }
    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            agent: agent(),
            limiter: RateLimiter::new(Duration::from_millis(250)),
        }
    }
}

impl ArtworkProvider for FanartTvProvider {
    fn id(&self) -> &'static str {
        "fanarttv"
    }
    fn needs_mbid(&self) -> bool {
        true
    }
    fn find_album_cover(&self, query: &ArtworkQuery) -> Result<Option<ArtworkResult>, String> {
        let Some(mbid) = &query.release_group_mbid else {
            return Ok(None);
        };
        let url = format!("{}/v3/music/albums/{mbid}", self.base_url);
        let value = self.limiter.run(|| {
            match self.agent.get(&url).query("api_key", &self.api_key).call() {
                Ok(response) => response
                    .into_json::<Value>()
                    .map(Some)
                    .map_err(|error| error.to_string()),
                Err(ureq::Error::Status(404, _)) => Ok(None),
                Err(error) => Err(http_error(error)),
            }
        })?;
        Ok(value.as_ref().and_then(|value| parse_fanart(value, mbid)))
    }
}

fn parse_fanart(value: &Value, release_group_mbid: &str) -> Option<ArtworkResult> {
    let cover = value
        .get("albums")?
        .get(release_group_mbid)?
        .get("albumcover")?
        .as_array()?
        .first()?
        .get("url")?
        .as_str()?;
    Some(ArtworkResult {
        provider: "fanarttv",
        image_url: cover.to_string(),
        width: None,
    })
}

// ---- Registry --------------------------------------------------------------

/// Artwork providers in priority order. Built from config; the fill pass asks it for a
/// cover and it tries each eligible provider until one hits.
pub struct ArtworkProviderRegistry {
    providers: Vec<Box<dyn ArtworkProvider>>,
}

impl ArtworkProviderRegistry {
    pub fn new(providers: Vec<Box<dyn ArtworkProvider>>) -> Self {
        Self { providers }
    }

    /// The default lane: MusicBrainz-id matches first (Cover Art Archive, then fanart.tv
    /// when an API key is configured), then text search (iTunes, Deezer).
    pub fn lane(fanart_tv_key: Option<String>) -> Self {
        let mut providers: Vec<Box<dyn ArtworkProvider>> =
            vec![Box::new(CoverArtArchiveProvider::new())];
        if let Some(key) = fanart_tv_key.filter(|key| !key.is_empty()) {
            providers.push(Box::new(FanartTvProvider::new(key)));
        }
        providers.push(Box::new(ItunesProvider::new()));
        providers.push(Box::new(DeezerProvider::new()));
        Self::new(providers)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Try each eligible provider in order; return the first confident cover.
    pub fn find_cover(&self, query: &ArtworkQuery) -> Option<ArtworkResult> {
        for provider in &self.providers {
            if provider.needs_mbid() && !query.has_mbid() {
                continue;
            }
            match provider.find_album_cover(query) {
                Ok(Some(result)) => return Some(result),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(provider = provider.id(), %error, "artwork provider failed");
                }
            }
        }
        None
    }

    /// Try each provider's artist-image search in order; return the first confident hit.
    pub fn find_artist(&self, query: &ArtistArtworkQuery) -> Option<ArtworkResult> {
        for provider in &self.providers {
            match provider.find_artist_image(query) {
                Ok(Some(result)) => return Some(result),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(provider = provider.id(), %error, "artist image provider failed");
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> ArtworkQuery {
        ArtworkQuery {
            artist: "Darkwood Dub".to_string(),
            album: "Paramparčad".to_string(),
            release_mbid: None,
            release_group_mbid: None,
        }
    }

    #[test]
    fn itunes_picks_matching_album_and_upgrades_resolution() {
        let value = serde_json::json!({
            "results": [
                { "artistName": "Other Band", "collectionName": "Other Album",
                  "artworkUrl100": "https://is/x/100x100bb.jpg" },
                { "artistName": "Darkwood Dub", "collectionName": "Paramparčad",
                  "artworkUrl100": "https://is/y/100x100bb.jpg" }
            ]
        });
        let result = parse_itunes(&value, &query()).expect("match");
        assert_eq!(result.provider, "itunes");
        assert_eq!(result.image_url, "https://is/y/600x600bb.jpg");
        assert_eq!(result.width, Some(600));
    }

    #[test]
    fn image_magic_bytes_accept_images_reject_others() {
        // ISS-28: validate by content, so an HTML/JSON error page isn't cached as art.
        assert_eq!(
            image_extension_from_magic(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("jpg")
        );
        assert_eq!(
            image_extension_from_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A]),
            Some("png")
        );
        assert_eq!(image_extension_from_magic(b"GIF89a..."), Some("gif"));
        assert_eq!(
            image_extension_from_magic(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("webp")
        );
        assert_eq!(image_extension_from_magic(b"<!DOCTYPE html><html>"), None);
        assert_eq!(image_extension_from_magic(b"{\"error\":\"nope\"}"), None);
        assert_eq!(image_extension_from_magic(b""), None);
    }

    #[test]
    fn itunes_does_not_claim_600_when_no_upgrade_happened() {
        // ISS-29: when the artworkUrl100 doesn't carry the 100x100bb size token the URL
        // can't be upgraded, so width must not falsely claim 600.
        let value = serde_json::json!({
            "results": [
                { "artistName": "Darkwood Dub", "collectionName": "Paramparčad",
                  "artworkUrl100": "https://is/y/source.jpg" }
            ]
        });
        let result = parse_itunes(&value, &query()).expect("match");
        assert_eq!(result.image_url, "https://is/y/source.jpg", "URL unchanged");
        assert_eq!(
            result.width,
            Some(100),
            "reports the actual 100px size, not 600"
        );
    }

    #[test]
    fn itunes_returns_none_when_nothing_matches() {
        let value = serde_json::json!({
            "results": [
                { "artistName": "Someone Else", "collectionName": "Different Record",
                  "artworkUrl100": "https://is/z/100x100bb.jpg" }
            ]
        });
        assert!(parse_itunes(&value, &query()).is_none());
    }

    #[test]
    fn deezer_picks_cover_xl() {
        let value = serde_json::json!({
            "data": [
                { "title": "Paramparčad", "artist": { "name": "Darkwood Dub" },
                  "cover_xl": "https://e-cdn/cover/xl.jpg" }
            ]
        });
        let result = parse_deezer(&value, &query()).expect("match");
        assert_eq!(result.provider, "deezer");
        assert_eq!(result.image_url, "https://e-cdn/cover/xl.jpg");
        assert_eq!(result.width, Some(1000));
    }

    #[test]
    fn deezer_artist_picks_matching_portrait() {
        let value = serde_json::json!({
            "data": [
                { "name": "Some Other Band", "picture_xl": "https://e-cdn/artist/other.jpg" },
                { "name": "Azra", "picture_xl": "https://e-cdn/artist/azra-xl.jpg" }
            ]
        });
        let query = ArtistArtworkQuery {
            name: "Azra".to_string(),
        };
        let result = parse_deezer_artist(&value, &query).expect("match");
        assert_eq!(result.provider, "deezer");
        assert_eq!(result.image_url, "https://e-cdn/artist/azra-xl.jpg");
    }

    #[test]
    fn deezer_artist_skips_silhouette_and_mismatch() {
        // A generic Deezer silhouette URL (empty id segment) is rejected…
        let silhouette = serde_json::json!({
            "data": [{ "name": "Azra", "picture_xl": "https://e-cdn/artist//xl.jpg" }]
        });
        let query = ArtistArtworkQuery {
            name: "Azra".to_string(),
        };
        assert!(parse_deezer_artist(&silhouette, &query).is_none());

        // …and a name that doesn't match is skipped.
        let mismatch = serde_json::json!({
            "data": [{ "name": "Completely Different", "picture_xl": "https://e-cdn/artist/x.jpg" }]
        });
        assert!(parse_deezer_artist(&mismatch, &query).is_none());
    }

    #[test]
    fn caa_prefers_the_front_image() {
        let value = serde_json::json!({
            "images": [
                { "front": false, "image": "https://caa/back.jpg" },
                { "front": true,  "image": "https://caa/front.jpg" }
            ]
        });
        let result = parse_caa(&value).expect("front");
        assert_eq!(result.image_url, "https://caa/front.jpg");
        assert_eq!(result.provider, "coverartarchive");
    }

    #[test]
    fn fanart_picks_the_first_album_cover() {
        let value = serde_json::json!({
            "albums": {
                "rg-mbid-1": {
                    "albumcover": [
                        { "id": "1", "url": "https://assets.fanart.tv/cover1.jpg", "likes": "5" },
                        { "id": "2", "url": "https://assets.fanart.tv/cover2.jpg", "likes": "1" }
                    ]
                }
            }
        });
        let result = parse_fanart(&value, "rg-mbid-1").expect("cover");
        assert_eq!(result.provider, "fanarttv");
        assert_eq!(result.image_url, "https://assets.fanart.tv/cover1.jpg");
    }

    #[test]
    fn registry_skips_mbid_only_providers_without_ids() {
        // A CAA-only registry with no MBIDs finds nothing (provider skipped, no network).
        let registry = ArtworkProviderRegistry::new(vec![Box::new(CoverArtArchiveProvider::new())]);
        assert!(registry.find_cover(&query()).is_none());
    }

    #[test]
    fn normalization_matches_loosely() {
        assert!(matches(
            "Darkwood Dub",
            "Paramparčad",
            "darkwood dub",
            "Paramparčad (Remaster)"
        ));
        assert!(!matches("A", "B", "C", "D"));
    }
}
