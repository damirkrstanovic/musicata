//! Synchronized multi-room playback via Snapcast (feature `snapcast`).
//!
//! Unlike every other Musicata playback path — where the endpoint fetches a per-track URL
//! and decodes it itself — Snapcast broadcasts one continuous, server-decoded PCM stream to
//! N sample-accurate clients. So this module owns the genuinely new server-side stage:
//!
//! - [`decode`] — symphonia decode of a library track → stereo → `rubato` resample to the
//!   fixed stream rate → interleaved `i16`.
//! - [`writer`] — a dedicated OS thread streaming that PCM into the FIFO snapserver reads;
//!   snapserver's real-time pipe pacing backpressures our blocking writes (no clock here).
//! - [`server`] — snapserver subprocess + FIFO lifecycle (the one "Musicata" pipe stream).
//! - [`control`] — JSON-RPC: enumerate clients (rooms), set per-output volume, point groups
//!   at our stream.
//!
//! The playback cursor — translating queue/seek/skip/pause into writer messages — lives in
//! `players.rs` (`SnapcastPlayer`), next to the other server-owned-queue players so the
//! queue semantics are shared. See `docs/snapcast.md`.

mod control;
mod decode;
mod server;
mod writer;

use std::sync::Arc;

use musicata_core::Track;
use musicata_storage::Database;
use tokio::sync::RwLock;

use crate::providers::ProviderRegistry;

pub use control::{SnapClient, SnapControl};
pub use decode::DecodedTrack;
pub use server::{STREAM_NAME, SnapcastManager, SnapcastSettings};
pub use writer::{WriterEvent, WriterMsg, run as run_writer};

/// Resolve a library track id to fully decoded stereo `i16` PCM at `target_rate`: look up
/// the track, read its encoded bytes from its provider, then decode off the async runtime.
pub async fn decode_queue_item(
    database: &Database,
    providers: &Arc<RwLock<ProviderRegistry>>,
    track_id: &str,
    target_rate: u32,
) -> Result<DecodedTrack, String> {
    let track = database
        .track(track_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("unknown track {track_id}"))?;
    let extension = track.extension.clone();
    let bytes = read_track_bytes(providers, &track).await?;
    tokio::task::spawn_blocking(move || decode::decode_track(&bytes, &extension, target_rate))
        .await
        .map_err(|error| error.to_string())?
}

/// Read a track's encoded bytes from whichever provider owns it (mirrors
/// `main.rs::read_track_source_file`: SMB reads by item id; everything else reads the path).
async fn read_track_bytes(
    providers: &Arc<RwLock<ProviderRegistry>>,
    track: &Track,
) -> Result<Vec<u8>, String> {
    #[cfg(feature = "provider-smb")]
    {
        if let Some(crate::providers::ProviderHandle::Smb(smb)) =
            providers.read().await.get(&track.provider.provider_id)
        {
            return smb
                .read_file(&track.provider.item_id)
                .await
                .map_err(|error| error.to_string());
        }
    }
    tokio::fs::read(&track.path)
        .await
        .map_err(|error| error.to_string())
}
