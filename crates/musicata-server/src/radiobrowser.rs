//! Client for the Radio Browser public directory (https://www.radio-browser.info),
//! a free, community-maintained database of internet radio stations (no API key).
//!
//! We proxy it server-side so LAN controllers without internet still get the
//! directory, and so we send one descriptive User-Agent and use the round-robin host
//! per Radio Browser's guidance. Playback uses each station's `url_resolved` (the
//! stream URL with playlists/redirects already followed).

use serde::{Deserialize, Serialize};

const DEFAULT_BASE: &str = "https://all.api.radio-browser.info";

/// A normalized directory station for the UI (distinct from a saved `RadioStation`).
#[derive(Clone, Debug, Serialize)]
pub struct DirectoryStation {
    pub name: String,
    pub stream_url: String,
    pub homepage_url: Option<String>,
    pub favicon: Option<String>,
    pub country: Option<String>,
    pub tags: Option<String>,
    pub codec: Option<String>,
    pub bitrate: u32,
}

#[derive(Debug, Deserialize)]
struct RbStation {
    name: String,
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    favicon: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate: u32,
}

#[derive(Clone)]
pub struct RadioBrowserClient {
    http: ureq::Agent,
    base: String,
}

impl Default for RadioBrowserClient {
    fn default() -> Self {
        Self::new(DEFAULT_BASE)
    }
}

impl RadioBrowserClient {
    pub fn new(base: &str) -> Self {
        let user_agent = format!("Musicata/{}", env!("CARGO_PKG_VERSION"));
        let http = ureq::AgentBuilder::new().user_agent(&user_agent).build();
        Self {
            http,
            base: base.to_string(),
        }
    }

    /// Search by name, or return the most-voted stations when `query` is empty.
    /// Blocking — call from `spawn_blocking`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<DirectoryStation>, String> {
        let stations: Vec<RbStation> = if query.trim().is_empty() {
            self.http
                .get(&format!("{}/json/stations/topvote/{limit}", self.base))
                .call()
                .map_err(request_error)?
                .into_json()
                .map_err(|error| error.to_string())?
        } else {
            self.http
                .get(&format!("{}/json/stations/search", self.base))
                .query("name", query)
                .query("limit", &limit.to_string())
                .query("hidebroken", "true")
                .query("order", "clickcount")
                .query("reverse", "true")
                .call()
                .map_err(request_error)?
                .into_json()
                .map_err(|error| error.to_string())?
        };

        Ok(stations
            .into_iter()
            .filter(|station| !station.url_resolved.is_empty())
            .map(|station| DirectoryStation {
                name: station.name,
                stream_url: station.url_resolved,
                homepage_url: optional(station.homepage),
                favicon: optional(station.favicon),
                country: optional(station.country),
                tags: optional(station.tags),
                codec: optional(station.codec),
                bitrate: station.bitrate,
            })
            .collect())
    }
}

fn optional(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn request_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("Radio Browser returned HTTP {status}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}
