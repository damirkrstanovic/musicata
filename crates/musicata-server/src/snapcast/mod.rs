// SPDX-License-Identifier: AGPL-3.0-or-later
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
mod dsp;
pub(crate) mod health;
mod library;
mod server;
mod stream;
mod writer;

pub use control::{SnapClient, SnapControl, SnapGroup, SnapStream};
pub use dsp::StereoEq;
pub(crate) use library::{LibraryStream, stream_track as stream_library_track};
pub use server::{
    AIRPLAY_STREAM_NAME, SPOTIFY_STREAM_NAME, STREAM_NAME, SnapRoom, SnapcastManager,
    SnapcastSettings, binary_present, install_command, validate_buffer_ms,
};
pub(crate) use stream::stream_radio;
pub use writer::{WriterEvent, WriterMsg, run as run_writer};
