//! Provider registry: one unified handle over every kind of music source.
//!
//! Sources are dispatched by an enum (`ProviderHandle`) rather than `dyn`, the same
//! way [`crate::players::PlayerHandle`] is — it keeps async methods object-safe and
//! makes adding a source a matter of one variant plus its match arms. Each provider
//! advertises [`ProviderCapabilities`] so callers can skip work a source can't do
//! (e.g. an internet-radio source can stream but never scans).

use std::sync::Arc;

use musicata_core::{
    BrowseEntry, Library, LocalDiskProvider, MusicProvider, ProviderCapabilities, ScanProgress,
    StreamSpec, merge_libraries, scan_local_library_incremental,
};
use musicata_storage::{Database, SourceRecord};

#[cfg(feature = "provider-archive")]
use crate::archive_org::ArchiveProvider;
#[cfg(feature = "provider-opensubsonic")]
use crate::opensubsonic::OpenSubsonicProvider;
#[cfg(feature = "provider-podcast")]
use crate::podcast::PodcastProvider;
#[cfg(feature = "provider-smb")]
use crate::smb::SmbProvider;

/// Build a provider from a persisted source record. Returns an error when the
/// source kind isn't supported by this build (e.g. an SMB share recorded by a
/// `provider-smb` build, opened by one compiled without it).
pub fn provider_from_record(record: &SourceRecord) -> anyhow::Result<ProviderHandle> {
    match record.kind.as_str() {
        #[cfg(feature = "provider-smb")]
        "smb" => Ok(ProviderHandle::Smb(std::sync::Arc::new(
            SmbProvider::from_record(record)?,
        ))),
        #[cfg(not(feature = "provider-smb"))]
        "smb" => anyhow::bail!(
            "SMB support is not compiled into this build (enable the `provider-smb` feature)"
        ),
        #[cfg(feature = "provider-opensubsonic")]
        "opensubsonic" => Ok(ProviderHandle::OpenSubsonic(std::sync::Arc::new(
            OpenSubsonicProvider::from_record(record)?,
        ))),
        #[cfg(not(feature = "provider-opensubsonic"))]
        "opensubsonic" => anyhow::bail!(
            "OpenSubsonic support is not compiled into this build (enable the `provider-opensubsonic` feature)"
        ),
        #[cfg(feature = "provider-podcast")]
        "podcast" => Ok(ProviderHandle::Podcast(std::sync::Arc::new(
            PodcastProvider::from_record(record)?,
        ))),
        #[cfg(not(feature = "provider-podcast"))]
        "podcast" => anyhow::bail!(
            "podcast support is not compiled into this build (enable the `provider-podcast` feature)"
        ),
        #[cfg(feature = "provider-archive")]
        "archive" => Ok(ProviderHandle::Archive(std::sync::Arc::new(
            ArchiveProvider::from_record(record)?,
        ))),
        #[cfg(not(feature = "provider-archive"))]
        "archive" => anyhow::bail!(
            "Internet Archive support is not compiled into this build (enable the `provider-archive` feature)"
        ),
        other => anyhow::bail!("unknown source kind: {other}"),
    }
}

/// The provider id for an SMB share — stable across restarts so its tracks keep
/// the same attribution. Human-readable rather than hashed.
pub fn smb_provider_id(host: &str, share: &str, base_path: &str) -> String {
    let base = base_path.trim_matches(['/', '\\']);
    let suffix = if base.is_empty() {
        String::new()
    } else {
        format!("/{}", base.replace('\\', "/"))
    };
    format!("smb:{}/{}{}", host.to_lowercase(), share, suffix)
}

/// The provider id for an upstream OpenSubsonic source — stable across restarts (so its tracks
/// keep their attribution) and distinct from `smb:`/`local-disk`/`radio`. Derived from the base
/// URL with the scheme stripped and lowercased, e.g. `opensubsonic:navidrome.example.com`.
#[cfg(feature = "provider-opensubsonic")]
pub fn opensubsonic_provider_id(base_url: &str) -> String {
    let without_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    format!("opensubsonic:{}", without_scheme.trim_end_matches('/').to_lowercase())
}

/// Stable id of the built-in internet-radio source. Like `local-disk`, it is always
/// present (not stored in the `sources` table) and can't be removed.
pub const RADIO_PROVIDER_ID: &str = "radio";

/// The internet-radio source: a non-scannable, browsable, streamable provider whose
/// catalogue is the user's saved stations (the `radio_stations` table). It never
/// merges into the scanned library — `browse` lists the stations and `resolve` turns
/// a station id into its stream URL.
#[derive(Clone)]
pub struct RadioProvider {
    database: Database,
}

impl RadioProvider {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::STREAM_ONLY
    }

    /// List the saved stations as browse entries (their stream URLs are known up
    /// front, so they're carried inline and a client can play without `resolve`).
    async fn browse(&self) -> anyhow::Result<Vec<BrowseEntry>> {
        let stations = self.database.list_radio_stations().await?;
        Ok(stations
            .into_iter()
            .map(|station| BrowseEntry {
                id: station.id,
                title: station.name,
                subtitle: station
                    .homepage_url
                    .as_deref()
                    .and_then(|url| url.split("://").nth(1).or(Some(url)))
                    .map(|host| host.trim_end_matches('/').to_string()),
                stream_url: Some(station.stream_url),
                homepage_url: station.homepage_url,
                is_container: false,
            })
            .collect())
    }

    /// Resolve a station id to its playable stream.
    async fn resolve(&self, item_id: &str) -> anyhow::Result<StreamSpec> {
        let station = self
            .database
            .radio_station(item_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown radio station: {item_id}"))?;
        Ok(StreamSpec {
            url: station.stream_url,
            title: Some(station.name),
            content_type: None,
        })
    }
}

/// A single configured music source.
#[derive(Clone)]
pub enum ProviderHandle {
    Local(Arc<LocalDiskProvider>),
    Radio(Arc<RadioProvider>),
    #[cfg(feature = "provider-smb")]
    Smb(Arc<SmbProvider>),
    #[cfg(feature = "provider-opensubsonic")]
    OpenSubsonic(Arc<OpenSubsonicProvider>),
    #[cfg(feature = "provider-podcast")]
    Podcast(Arc<PodcastProvider>),
    #[cfg(feature = "provider-archive")]
    Archive(Arc<ArchiveProvider>),
}

impl ProviderHandle {
    pub fn local(provider: LocalDiskProvider) -> Self {
        ProviderHandle::Local(Arc::new(provider))
    }

    pub fn radio(provider: RadioProvider) -> Self {
        ProviderHandle::Radio(Arc::new(provider))
    }

    pub fn provider_id(&self) -> String {
        match self {
            ProviderHandle::Local(provider) => provider.provider_id().to_string(),
            ProviderHandle::Radio(_) => RADIO_PROVIDER_ID.to_string(),
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(provider) => provider.provider_id().clone(),
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(provider) => provider.provider_id().clone(),
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(provider) => provider.provider_id().clone(),
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(provider) => provider.provider_id().clone(),
        }
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        match self {
            ProviderHandle::Local(provider) => provider.capabilities(),
            ProviderHandle::Radio(provider) => provider.capabilities(),
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(_) => ProviderCapabilities::DISK,
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(_) => ProviderCapabilities::DISK,
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(provider) => provider.capabilities(),
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(provider) => provider.capabilities(),
        }
    }

    /// Cheaply verify the source is reachable and readable (connect + list a
    /// directory) before committing to a full scan. Local disk and radio (a DB-backed
    /// station list) are always ready.
    pub async fn validate(&self) -> anyhow::Result<()> {
        match self {
            ProviderHandle::Local(_) | ProviderHandle::Radio(_) => Ok(()),
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(provider) => provider.validate().await,
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(provider) => provider.validate().await,
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(provider) => provider.validate().await,
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(provider) => provider.validate().await,
        }
    }

    /// Browse a non-scannable source's catalogue (e.g. internet-radio stations).
    /// Scannable sources merge into the library and are browsed through the library
    /// endpoints, so they return an error here.
    pub async fn browse(&self) -> anyhow::Result<Vec<BrowseEntry>> {
        match self {
            ProviderHandle::Radio(provider) => provider.browse().await,
            ProviderHandle::Local(_) => {
                anyhow::bail!("the local library is browsed through the library endpoints")
            }
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(_) => {
                anyhow::bail!("SMB shares are browsed through the library endpoints")
            }
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(_) => {
                anyhow::bail!("OpenSubsonic sources are browsed through the library endpoints")
            }
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(provider) => provider.browse().await,
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(provider) => provider.browse().await,
        }
    }

    /// Resolve an item id within this source to a playable stream. Only meaningful
    /// for sources whose items aren't already library tracks (internet radio); other
    /// sources stream library tracks through `/api/tracks/{id}/stream`.
    pub async fn resolve(&self, item_id: &str) -> anyhow::Result<StreamSpec> {
        match self {
            ProviderHandle::Radio(provider) => provider.resolve(item_id).await,
            ProviderHandle::Local(_) => {
                anyhow::bail!("local tracks are streamed through /api/tracks/{{id}}/stream")
            }
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(_) => {
                anyhow::bail!("SMB tracks are streamed through /api/tracks/{{id}}/stream")
            }
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(_) => {
                anyhow::bail!("OpenSubsonic tracks are streamed through /api/tracks/{{id}}/stream")
            }
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(provider) => provider.resolve(item_id).await,
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(provider) => provider.resolve(item_id).await,
        }
    }

    /// Scan this source's catalogue into a [`Library`] (full scan). Scanning is
    /// blocking work (disk or network I/O + tag parsing), so it runs on a blocking
    /// thread.
    pub async fn scan(&self) -> anyhow::Result<Library> {
        self.scan_with_progress(None, |_| {}).await
    }

    /// As [`Self::scan`], but incremental: `prior` (the stored library) lets the
    /// scanner reuse unchanged files' metadata and read tags only for new/changed
    /// files. Reports [`ScanProgress`] as it goes.
    pub async fn scan_with_progress<F>(
        &self,
        prior: Option<Arc<Library>>,
        mut progress: F,
    ) -> anyhow::Result<Library>
    where
        F: FnMut(ScanProgress) + Send + 'static,
    {
        match self {
            ProviderHandle::Local(provider) => {
                let root = provider.root().to_path_buf();
                let scanned = tokio::task::spawn_blocking(move || {
                    scan_local_library_incremental(&root, prior.as_deref(), &mut progress)
                })
                .await??;
                Ok(scanned)
            }
            // Radio has no scannable catalogue; `scan_all` skips it via `can_scan`,
            // and a direct rescan request is rejected rather than silently no-op'd.
            ProviderHandle::Radio(_) => {
                anyhow::bail!("the internet-radio source has no scannable catalogue")
            }
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(provider) => provider.scan_with_progress(prior, progress).await,
            #[cfg(feature = "provider-opensubsonic")]
            ProviderHandle::OpenSubsonic(provider) => {
                provider.scan_with_progress(prior, progress).await
            }
            // A podcast feed has no scannable catalogue; it's browsed + resolved like radio.
            #[cfg(feature = "provider-podcast")]
            ProviderHandle::Podcast(_) => {
                anyhow::bail!("a podcast source has no scannable catalogue")
            }
            #[cfg(feature = "provider-archive")]
            ProviderHandle::Archive(_) => {
                anyhow::bail!("an Internet Archive source has no scannable catalogue")
            }
        }
    }
}

/// The set of active sources. Held behind a lock in `AppState` so the
/// `/api/sources` handlers can add or remove sources at runtime.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Vec<ProviderHandle>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, provider: ProviderHandle) {
        let id = provider.provider_id();
        self.providers
            .retain(|existing| existing.provider_id() != id);
        self.providers.push(provider);
    }

    pub fn remove(&mut self, provider_id: &str) {
        self.providers
            .retain(|existing| existing.provider_id() != provider_id);
    }

    /// Look up an active source by id (used by the SMB streaming path and the
    /// generic browse/resolve endpoints).
    pub fn get(&self, provider_id: &str) -> Option<ProviderHandle> {
        self.providers
            .iter()
            .find(|provider| provider.provider_id() == provider_id)
            .cloned()
    }

    /// A snapshot of the active source handles, for scanning each in turn.
    pub fn handles(&self) -> Vec<ProviderHandle> {
        self.providers.clone()
    }

    /// Scan every scannable source and merge the results into one library. A source
    /// that fails to scan (e.g. an offline share) is logged and skipped rather than
    /// failing the whole library — the others still load.
    pub async fn scan_all(&self) -> anyhow::Result<Library> {
        let mut libraries = Vec::new();
        for provider in &self.providers {
            if !provider.capabilities().can_scan {
                continue;
            }
            match provider.scan().await {
                Ok(library) => libraries.push(library),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider.provider_id(),
                        %error,
                        "source scan failed; skipping it for this pass"
                    );
                }
            }
        }
        Ok(merge_libraries(libraries))
    }
}
