// SPDX-License-Identifier: AGPL-3.0-or-later
//! The FIFO writer: a dedicated OS thread that streams decoded PCM into the named
//! pipe snapserver reads. snapserver paces its pipe reads to real time (see
//! `../snapcast` `asio_stream.hpp` `nextTick_`), so our **blocking writes backpressure
//! automatically** — there is no clock here. Pausing simply stops writing (snapserver's
//! stream goes idle → silence to clients); seeking/skipping repositions the cursor.
//!
//! The thread is intentionally async-free: it owns the blocking pipe `File` and talks to
//! the async control task (`super::SnapcastPlayer`) only through channels — commands in
//! over a `std::sync::mpsc` (so it can block waiting for one while idle), events out over
//! a tokio unbounded channel (whose `send` is callable from any thread).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use super::decode::{CHANNELS, DecodedTrack};
use super::dsp::StereoEq;

/// PCM frames written per `write_all`. snapserver reads its pipe in small chunks paced to
/// real time (its `chunk_ms`, default 20 ms); feeding in **matching ~20 ms granularity** keeps
/// its input smooth, where coarse chunks make the pipe sawtooth and underrun between writes.
/// 960 frames = 20 ms at 48 kHz. Also small enough that seek/skip/pause react within a chunk.
const CHUNK_FRAMES: usize = 960;

/// How far ahead of real time we let the writer get before it sleeps. snapserver relays the
/// pipe to each snapclient, which buffers ~1 s and plays at the server-stamped presentation
/// time — snapserver does **not** backpressure our pipe writes. So we (a) burst up to this far
/// ahead at the start to prime the client buffer, then (b) feed at real time to keep it full.
/// Matching snapserver's default ~1 s client buffer primes it without overflowing; combined
/// with small (20 ms) chunks the feed is smooth. It is also the runaway cap that stops us
/// spinning a CPU when no client is connected (snapserver then drains the pipe greedily).
const PACING_LEAD: Duration = Duration::from_millis(1000);

/// One decoded track queued for output, plus the playback cursor into it (in frames) and
/// a per-track linear gain (volume leveling — see Phase 4 in docs/snapcast.md).
struct Loaded {
    track: Arc<DecodedTrack>,
    cursor_frames: usize,
    gain: f32,
}

/// Commands from the async control task to the writer thread.
pub enum WriterMsg {
    /// Start (or restart) at this track at `start_frame`, replacing whatever was
    /// playing and clearing any preloaded next track. `gain` is the per-track leveling
    /// factor (1.0 = unchanged).
    Load {
        track: Arc<DecodedTrack>,
        start_frame: usize,
        gain: f32,
    },
    /// Queue the next track for gapless continuation when the current one drains.
    Preload { track: Arc<DecodedTrack>, gain: f32 },
    /// Begin/stop writing PCM. Paused = snapserver stream goes idle (silence).
    SetPlaying(bool),
    /// Master output volume (0–100%), applied live to every client uniformly. Per-room
    /// trim is done via snapserver (the JSON-RPC control client / admin).
    SetVolume(u8),
    /// Reposition the current track's cursor (seek), in frames from the start.
    Seek { frame: usize },
    /// Replace the active EQ correction (or clear it with `None`). Applied per chunk before the
    /// FIFO — server-side correction for Snapcast, in-process (no CamillaDSP subprocess).
    SetDsp(Box<Option<StereoEq>>),
    /// Stop and clear everything (Stop / Clear).
    Stop,
    /// Tear the thread down.
    Shutdown,
}

/// Events from the writer thread back to the async control task.
pub enum WriterEvent {
    /// The current track drained and we rolled gaplessly into the preloaded next one.
    Advanced,
    /// The current track drained and nothing was preloaded.
    Drained,
}

/// Run the writer loop on the calling (dedicated) thread until `Shutdown` or the command
/// channel disconnects. Opening the FIFO for writing blocks until snapserver opens the
/// read end — by which point the stream is ready.
pub fn run(fifo_path: PathBuf, rx: Receiver<WriterMsg>, events: UnboundedSender<WriterEvent>) {
    let mut file = match OpenOptions::new().write(true).open(&fifo_path) {
        Ok(file) => file,
        Err(error) => {
            tracing::error!(path = %fifo_path.display(), %error, "snapcast: cannot open FIFO");
            return;
        }
    };
    let mut current: Option<Loaded> = None;
    let mut next: Option<(Arc<DecodedTrack>, f32)> = None;
    let mut playing = false;
    // Master volume as a linear factor (0.0–1.0); defaults to full scale.
    let mut volume = 1.0f32;
    // Optional server-side EQ correction applied per chunk (stateful per channel).
    let mut eq: Option<StereoEq> = None;
    // Wall-clock time the *next* frame to be written should reach the stream — the
    // real-time playout clock. `None` whenever we're idle (reset so a resume starts fresh,
    // not trying to "catch up" across the silent gap). The sample rate is fixed per stream.
    let mut clock: Option<Instant> = None;
    let mut scratch: Vec<u8> = Vec::with_capacity(CHUNK_FRAMES * CHANNELS * 2);

    loop {
        // Apply every pending command without blocking.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if !apply(
                        msg,
                        &mut current,
                        &mut next,
                        &mut playing,
                        &mut volume,
                        &mut eq,
                    ) {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Nothing to play — block for the next command instead of spinning. Drop the
        // playout clock so a resume restarts from now rather than racing to catch up.
        if !playing || current.is_none() {
            clock = None;
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(msg) => {
                    if !apply(
                        msg,
                        &mut current,
                        &mut next,
                        &mut playing,
                        &mut volume,
                        &mut eq,
                    ) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        // Write one chunk from the current track, then self-pace to real time.
        let (drained, frames_written, sample_rate) = {
            let loaded = current.as_mut().expect("current checked above");
            let total = loaded.track.frames();
            let start = loaded.cursor_frames.min(total);
            let end = (start + CHUNK_FRAMES).min(total);
            // Combine the per-track leveling gain with the live master volume. When both
            // are unity and there's no EQ this is a plain copy; otherwise each frame is
            // (optionally) EQ-filtered then scaled + clamped.
            let factor = loaded.gain * volume;
            let chunk = &loaded.track.samples[start * CHANNELS..end * CHANNELS];
            scratch.clear();
            if eq.is_none() && (factor - 1.0).abs() < f32::EPSILON {
                for sample in chunk {
                    scratch.extend_from_slice(&sample.to_le_bytes());
                }
            } else {
                let f = factor as f64;
                for frame in chunk.chunks(CHANNELS) {
                    let (mut l, mut r) =
                        (frame[0] as f64, frame.get(1).copied().unwrap_or(0) as f64);
                    if let Some(eq) = eq.as_mut() {
                        (l, r) = eq.process_frame(l, r);
                    }
                    let q = |v: f64| (v * f).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    scratch.extend_from_slice(&q(l).to_le_bytes());
                    scratch.extend_from_slice(&q(r).to_le_bytes());
                }
            }
            if let Err(error) = file.write_all(&scratch) {
                // snapserver likely restarted; try to reopen the FIFO once, then re-write
                // this chunk so we don't silently drop ~20 ms of audio.
                tracing::warn!(%error, "snapcast: FIFO write failed; reopening");
                match OpenOptions::new().write(true).open(&fifo_path) {
                    Ok(mut reopened) => {
                        if let Err(error) = reopened.write_all(&scratch) {
                            tracing::error!(%error, "snapcast: FIFO write failed after reopen; writer stopping");
                            return;
                        }
                        file = reopened;
                    }
                    Err(error) => {
                        tracing::error!(%error, "snapcast: FIFO reopen failed; writer stopping");
                        return;
                    }
                }
            }
            // Advance only after the chunk is actually written.
            loaded.cursor_frames = end;
            (end >= total, end - start, loaded.track.sample_rate)
        };

        // Pace to real time: keep the playout clock at most PACING_LEAD ahead of now (see the
        // const's doc). Bursts to prime the client buffer at the start, then feeds at real
        // time; also the runaway cap that stops a CPU spin when no client is connected.
        if frames_written > 0 && sample_rate > 0 {
            let chunk = Duration::from_secs_f64(frames_written as f64 / sample_rate as f64);
            let slot = clock.get_or_insert_with(Instant::now);
            let deadline = *slot + chunk;
            *slot = deadline;
            let target = deadline.checked_sub(PACING_LEAD).unwrap_or(deadline);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }

        if drained {
            if let Some((track, gain)) = next.take() {
                current = Some(Loaded {
                    track,
                    cursor_frames: 0,
                    gain,
                });
                let _ = events.send(WriterEvent::Advanced);
            } else {
                current = None;
                let _ = events.send(WriterEvent::Drained);
            }
        }
    }
}

/// Apply one command to the writer's local state. Returns `false` on `Shutdown`.
fn apply(
    msg: WriterMsg,
    current: &mut Option<Loaded>,
    next: &mut Option<(Arc<DecodedTrack>, f32)>,
    playing: &mut bool,
    volume: &mut f32,
    eq: &mut Option<StereoEq>,
) -> bool {
    match msg {
        WriterMsg::Load {
            track,
            start_frame,
            gain,
        } => {
            let cursor_frames = start_frame.min(track.frames());
            *current = Some(Loaded {
                track,
                cursor_frames,
                gain,
            });
            *next = None;
            if let Some(eq) = eq.as_mut() {
                eq.reset(); // don't bleed the previous track's filter state into the new one
            }
        }
        WriterMsg::Preload { track, gain } => *next = Some((track, gain)),
        WriterMsg::SetPlaying(value) => *playing = value,
        WriterMsg::SetVolume(percent) => *volume = (percent.min(100) as f32) / 100.0,
        WriterMsg::Seek { frame } => {
            if let Some(loaded) = current.as_mut() {
                loaded.cursor_frames = frame.min(loaded.track.frames());
            }
            if let Some(eq) = eq.as_mut() {
                eq.reset();
            }
        }
        WriterMsg::SetDsp(chain) => *eq = *chain,
        WriterMsg::Stop => {
            *current = None;
            *next = None;
            *playing = false;
        }
        WriterMsg::Shutdown => return false,
    }
    true
}
