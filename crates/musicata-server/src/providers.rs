//! Provider registry: one unified handle over every kind of music source.
//!
//! Sources are dispatched by an enum (`ProviderHandle`) rather than `dyn`, the same
//! way [`crate::players::PlayerHandle`] is — it keeps async methods object-safe and
//! makes adding a source a matter of one variant plus its match arms. Each provider
//! advertises [`ProviderCapabilities`] so callers can skip work a source can't do
//! (e.g. an internet-radio source can stream but never scans).

use std::sync::Arc;

use musicata_core::{
    Library, LocalDiskProvider, MusicProvider, ProviderCapabilities, merge_libraries,
};
use musicata_storage::SourceRecord;

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

/// A single configured music source.
#[derive(Clone)]
pub enum ProviderHandle {
    Local(Arc<LocalDiskProvider>),
    #[cfg(feature = "provider-smb")]
    Smb(Arc<SmbProvider>),
}

impl ProviderHandle {
    pub fn local(provider: LocalDiskProvider) -> Self {
        ProviderHandle::Local(Arc::new(provider))
    }

    pub fn provider_id(&self) -> String {
        match self {
            ProviderHandle::Local(provider) => provider.provider_id().to_string(),
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(provider) => provider.provider_id().clone(),
        }
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        match self {
            ProviderHandle::Local(provider) => provider.capabilities(),
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(_) => ProviderCapabilities::DISK,
        }
    }

    /// Scan this source's catalogue into a [`Library`]. Scanning is blocking work
    /// (disk or network I/O + tag parsing), so it always runs on a blocking thread.
    pub async fn scan(&self) -> anyhow::Result<Library> {
        match self {
            ProviderHandle::Local(provider) => {
                let provider = provider.clone();
                let scanned =
                    tokio::task::spawn_blocking(move || provider.scan()).await??;
                Ok(scanned)
            }
            #[cfg(feature = "provider-smb")]
            ProviderHandle::Smb(provider) => provider.scan().await,
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
        self.providers.retain(|existing| existing.provider_id() != id);
        self.providers.push(provider);
    }

    pub fn remove(&mut self, provider_id: &str) {
        self.providers
            .retain(|existing| existing.provider_id() != provider_id);
    }

    pub fn get(&self, provider_id: &str) -> Option<ProviderHandle> {
        self.providers
            .iter()
            .find(|provider| provider.provider_id() == provider_id)
            .cloned()
    }

    pub fn handles(&self) -> Vec<ProviderHandle> {
        self.providers.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
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
