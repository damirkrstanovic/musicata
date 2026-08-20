// SPDX-License-Identifier: AGPL-3.0-or-later
//! Podcast RSS source: a browse-only ([`ProviderCapabilities::STREAM_ONLY`]) provider
//! whose catalogue is the episodes of one podcast feed.
//!
//! Like the internet-radio source (`crate::providers::RadioProvider`), a podcast feed
//! never merges into the scanned library — its episodes are streams, not files. The feed
//! URL is stored in the source's `host` column (reusing the OpenSubsonic field
//! convention); `browse` fetches + parses the feed into episode entries (their enclosure
//! URLs carried inline so a client can play without `resolve`), and `resolve` maps an
//! episode id back to its stream. The feed is fetched on demand, never on the scan hot
//! path. See `docs/decisions.md` (2026-06-27) for why podcasts went in before Jamendo.

use std::time::Duration;

use musicata_core::{BrowseEntry, ProviderCapabilities, StreamSpec};
use musicata_storage::SourceRecord;
use serde::Deserialize;

/// How long to wait on the feed host before giving up.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Bootstrap config for a podcast source: just the feed URL.
pub struct PodcastConfig {
    pub feed_url: String,
}

impl PodcastConfig {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let raw = record
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .ok_or_else(|| anyhow::anyhow!("podcast source needs a feed URL"))?;
        // Default to https:// when the user typed a bare host (mirrors OpenSubsonic).
        let feed_url = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        Ok(Self { feed_url })
    }
}

/// Stable provider id for a podcast source — derived from the feed URL with the scheme
/// stripped and lowercased, e.g. `podcast:feeds.example.com/show.xml`. Distinct from
/// `local-disk`/`radio`/`smb:`/`opensubsonic:`.
pub fn podcast_provider_id(feed_url: &str) -> String {
    let without_scheme = feed_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(feed_url);
    format!(
        "podcast:{}",
        without_scheme.trim_end_matches('/').to_lowercase()
    )
}

// ---- Parsing (pure, unit-tested) ----

/// The parsed, provider-neutral view of a podcast feed.
#[derive(Debug, Clone, PartialEq)]
pub struct PodcastFeed {
    pub title: Option<String>,
    pub image_url: Option<String>,
    pub episodes: Vec<PodcastEpisode>,
}

/// One playable podcast episode.
#[derive(Debug, Clone, PartialEq)]
pub struct PodcastEpisode {
    /// Stable id: the feed's `<guid>` when present, else the enclosure URL.
    pub id: String,
    pub title: String,
    pub stream_url: String,
    pub mime: Option<String>,
    pub published: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Rss {
    channel: Channel,
}

#[derive(Debug, Deserialize)]
struct Channel {
    #[serde(default)]
    title: Option<String>,
    // quick-xml strips namespace prefixes, so both `<image><url>` and the iTunes
    // `<itunes:image href="...">` deserialize into this one field.
    #[serde(default)]
    image: Option<RssImage>,
    #[serde(rename = "item", default)]
    items: Vec<RssItem>,
}

#[derive(Debug, Deserialize)]
struct RssImage {
    /// RSS 2.0 `<image><url>...</url></image>`.
    #[serde(default)]
    url: Option<String>,
    /// iTunes `<itunes:image href="..."/>` (artwork as an attribute).
    #[serde(rename = "@href", default)]
    href: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RssItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    guid: Option<Guid>,
    #[serde(default)]
    enclosure: Option<Enclosure>,
    #[serde(rename = "pubDate", default)]
    pub_date: Option<String>,
}

/// `<guid>` may carry an `isPermaLink` attribute, so its text lives under `$text`.
#[derive(Debug, Deserialize)]
struct Guid {
    #[serde(rename = "$text", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Enclosure {
    // Optional so a malformed `<enclosure>` (no `url`) skips just that item rather than
    // failing the whole feed deserialization.
    #[serde(rename = "@url", default)]
    url: Option<String>,
    #[serde(rename = "@type", default)]
    mime: Option<String>,
}

/// Trim and drop-if-empty, so blank XML elements become `None`.
fn cleaned(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a podcast RSS document into a [`PodcastFeed`]. Items without a playable
/// `<enclosure url=...>` are skipped — a feed can carry non-audio items.
pub fn parse_feed(xml: &str) -> anyhow::Result<PodcastFeed> {
    let rss: Rss = quick_xml::de::from_str(xml)
        .map_err(|error| anyhow::anyhow!("parse podcast feed: {error}"))?;
    let channel = rss.channel;
    let episodes = channel
        .items
        .into_iter()
        .filter_map(|item| {
            let enclosure = item.enclosure?;
            let stream_url = cleaned(enclosure.url)?;
            let id = cleaned(item.guid.and_then(|guid| guid.value))
                .unwrap_or_else(|| stream_url.clone());
            let title = cleaned(item.title).unwrap_or_else(|| "Untitled episode".to_string());
            Some(PodcastEpisode {
                id,
                title,
                stream_url,
                mime: cleaned(enclosure.mime),
                published: cleaned(item.pub_date),
            })
        })
        .collect();
    let image_url = cleaned(channel.image.and_then(|image| image.url.or(image.href)));
    Ok(PodcastFeed {
        title: cleaned(channel.title),
        image_url,
        episodes,
    })
}

// ---- Provider ----

/// A subscribed podcast feed, surfaced as a browse-only source.
pub struct PodcastProvider {
    provider_id: String,
    feed_url: String,
}

impl PodcastProvider {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let config = PodcastConfig::from_record(record)?;
        Ok(Self {
            provider_id: podcast_provider_id(&config.feed_url),
            feed_url: config.feed_url,
        })
    }

    pub fn provider_id(&self) -> &String {
        &self.provider_id
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::STREAM_ONLY
    }

    /// Fetch + parse the feed off the async runtime (blocking HTTP + parse).
    async fn fetch_feed(&self) -> anyhow::Result<PodcastFeed> {
        let url = self.feed_url.clone();
        tokio::task::spawn_blocking(move || {
            let user_agent = format!("Musicata/{}", env!("CARGO_PKG_VERSION"));
            let agent = ureq::AgentBuilder::new()
                .user_agent(&user_agent)
                .timeout(TIMEOUT)
                .build();
            let body = agent
                .get(&url)
                .call()
                .map_err(|error| anyhow::anyhow!("fetch podcast feed: {error}"))?
                .into_string()
                .map_err(|error| anyhow::anyhow!("read podcast feed: {error}"))?;
            parse_feed(&body)
        })
        .await?
    }

    /// Cheap reachability check: fetch + parse the feed once.
    pub async fn validate(&self) -> anyhow::Result<()> {
        self.fetch_feed().await.map(|_| ())
    }

    /// List the feed's episodes as browse entries (enclosure URLs carried inline).
    pub async fn browse(&self) -> anyhow::Result<Vec<BrowseEntry>> {
        let feed = self.fetch_feed().await?;
        let show = feed.title;
        Ok(feed
            .episodes
            .into_iter()
            .map(|episode| BrowseEntry {
                id: episode.id,
                title: episode.title,
                subtitle: episode.published.or_else(|| show.clone()),
                stream_url: Some(episode.stream_url),
                homepage_url: None,
                is_container: false,
            })
            .collect())
    }

    /// Resolve an episode id to its stream. Re-fetches the feed and matches by id;
    /// most clients play the inline `stream_url` from `browse` and never call this.
    pub async fn resolve(&self, item_id: &str) -> anyhow::Result<StreamSpec> {
        let feed = self.fetch_feed().await?;
        let episode = feed
            .episodes
            .into_iter()
            .find(|episode| episode.id == item_id)
            .ok_or_else(|| anyhow::anyhow!("unknown podcast episode: {item_id}"))?;
        Ok(StreamSpec {
            url: episode.stream_url,
            title: Some(episode.title),
            content_type: episode.mime,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
        <rss version="2.0">
          <channel>
            <title>Example Show</title>
            <image><url>https://cdn.example.com/cover.jpg</url></image>
            <item>
              <title>Episode One</title>
              <guid isPermaLink="false">ep-0001</guid>
              <pubDate>Mon, 01 Jun 2026 10:00:00 +0000</pubDate>
              <enclosure url="https://cdn.example.com/ep1.mp3" type="audio/mpeg" length="42"/>
            </item>
            <item>
              <title>Episode Two (no guid)</title>
              <enclosure url="https://cdn.example.com/ep2.mp3" type="audio/mpeg"/>
            </item>
            <item>
              <title>A note, no audio</title>
            </item>
          </channel>
        </rss>"#;

    #[test]
    fn parses_channel_and_episodes() {
        let feed = parse_feed(FEED).expect("parse");
        assert_eq!(feed.title.as_deref(), Some("Example Show"));
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://cdn.example.com/cover.jpg")
        );
        // The text-only item (no enclosure) is dropped; two playable episodes remain.
        assert_eq!(feed.episodes.len(), 2);

        let first = &feed.episodes[0];
        assert_eq!(first.id, "ep-0001"); // from <guid>
        assert_eq!(first.title, "Episode One");
        assert_eq!(first.stream_url, "https://cdn.example.com/ep1.mp3");
        assert_eq!(first.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(
            first.published.as_deref(),
            Some("Mon, 01 Jun 2026 10:00:00 +0000")
        );

        // No <guid> → the id falls back to the enclosure URL.
        assert_eq!(feed.episodes[1].id, "https://cdn.example.com/ep2.mp3");
    }

    #[test]
    fn channel_image_falls_back_to_itunes_image() {
        // ISS-25: feeds that supply art only via <itunes:image href="..."> still get it.
        let feed_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss version="2.0">
              <channel>
                <title>Show</title>
                <itunes:image href="https://cdn.example.com/cover.jpg"/>
              </channel>
            </rss>"#;
        let feed = parse_feed(feed_xml).expect("parse");
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://cdn.example.com/cover.jpg")
        );
    }

    #[test]
    fn rejects_non_rss() {
        assert!(parse_feed("<html><body>not a feed</body></html>").is_err());
    }

    #[test]
    fn malformed_enclosure_skips_only_that_item() {
        // BUG-10: a present-but-url-less <enclosure> must not fail the whole feed —
        // it should be skipped like any non-playable item (per the doc comment).
        let feed_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss version="2.0">
              <channel>
                <title>Show</title>
                <item>
                  <title>Broken enclosure</title>
                  <enclosure type="audio/mpeg" length="1"/>
                </item>
                <item>
                  <title>Good one</title>
                  <enclosure url="https://cdn.example.com/ok.mp3" type="audio/mpeg"/>
                </item>
              </channel>
            </rss>"#;
        let feed = parse_feed(feed_xml).expect("a malformed enclosure must not fail the feed");
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(
            feed.episodes[0].stream_url,
            "https://cdn.example.com/ok.mp3"
        );
    }

    #[test]
    fn provider_id_strips_scheme_and_lowercases() {
        assert_eq!(
            podcast_provider_id("https://Feeds.Example.com/Show.xml/"),
            "podcast:feeds.example.com/show.xml"
        );
    }

    #[test]
    fn config_defaults_scheme_and_requires_url() {
        let with_host = SourceRecord {
            id: "x".into(),
            kind: "podcast".into(),
            display_name: "Show".into(),
            enabled: true,
            host: Some("feeds.example.com/show.xml".into()),
            share: None,
            base_path: None,
            username: None,
            password: None,
            domain: None,
            created_at_unix_seconds: 0,
        };
        assert_eq!(
            PodcastConfig::from_record(&with_host).unwrap().feed_url,
            "https://feeds.example.com/show.xml"
        );

        let no_host = SourceRecord {
            host: None,
            ..with_host
        };
        assert!(PodcastConfig::from_record(&no_host).is_err());
    }

    /// Live browse against a real feed. Ignored by default (hits the network); run with
    /// `--ignored` and `MUSICATA_PODCAST_FEED=<url>`.
    #[tokio::test]
    #[ignore = "hits the network; set MUSICATA_PODCAST_FEED"]
    async fn podcast_live_browse() {
        let url = std::env::var("MUSICATA_PODCAST_FEED")
            .expect("set MUSICATA_PODCAST_FEED to a podcast RSS URL");
        let record = SourceRecord {
            id: String::new(),
            kind: "podcast".into(),
            display_name: "Live".into(),
            enabled: true,
            host: Some(url),
            share: None,
            base_path: None,
            username: None,
            password: None,
            domain: None,
            created_at_unix_seconds: 0,
        };
        let provider = PodcastProvider::from_record(&record).expect("build provider");
        let entries = provider.browse().await.expect("browse live feed");
        assert!(!entries.is_empty(), "feed should have episodes");
        assert!(entries.iter().all(|entry| entry.stream_url.is_some()));
    }
}
