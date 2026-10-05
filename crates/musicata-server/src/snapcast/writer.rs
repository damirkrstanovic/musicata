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
use musicata_core::pcm_dsp::{AudioTap, StereoMeter};
use std::sync::atomic::Ordering;

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
    generation: u64,
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
        generation: u64,
    },
    /// Queue the next track for gapless continuation when the current one drains.
    Preload {
        track: Arc<DecodedTrack>,
        gain: f32,
        generation: u64,
    },
    /// Begin/stop writing PCM. Paused = snapserver stream goes idle (silence).
    SetPlaying(bool),
    /// Master output volume (0–100%), applied live to every client uniformly. Per-room
    /// trim is done via snapserver (the JSON-RPC control client / admin).
    SetVolume(u8),
    /// Reposition the current track's cursor (seek), in frames from the start.
    Seek { frame: usize },
    /// Replace the active EQ correction (or clear it with `None`). Applied per chunk before the
    /// FIFO — server-side correction for Snapcast, in-process (no CamillaDSP subprocess).
    SetDsp {
        chain: Box<Option<StereoEq>>,
        revision: u64,
    },
    /// Stop and clear everything (Stop / Clear).
    Stop,
    /// Tear the thread down.
    Shutdown,
}

/// Events from the writer thread back to the async control task.
pub enum WriterEvent {
    /// The current track drained and we rolled gaplessly into the preloaded next one.
    Advanced { generation: u64 },
    /// The current track drained and nothing was preloaded.
    Drained { generation: u64 },
}

/// Run the writer loop on the calling (dedicated) thread until `Shutdown` or the command
/// channel disconnects. Opening the FIFO for writing blocks until snapserver opens the
/// read end — by which point the stream is ready.
pub fn run(
    fifo_path: PathBuf,
    rx: Receiver<WriterMsg>,
    events: UnboundedSender<WriterEvent>,
    tap: Arc<AudioTap>,
) {
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
    let mut meter = StereoMeter::default();
    let mut meter_frames = 0;
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
                        &mut meter,
                        &tap,
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
                        &mut meter,
                        &tap,
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
                let revision = tap.applied_revision.swap(0, Ordering::AcqRel);
                tap.clear();
                meter.reset();
                meter_frames = 0;
                tracing::warn!(%error, "snapcast: FIFO write failed; reopening");
                match OpenOptions::new().write(true).open(&fifo_path) {
                    Ok(mut reopened) => {
                        if let Err(error) = reopened.write_all(&scratch) {
                            tracing::error!(%error, "snapcast: FIFO write failed after reopen; writer stopping");
                            return;
                        }
                        file = reopened;
                        tap.applied_revision.store(revision, Ordering::Release);
                    }
                    Err(error) => {
                        tracing::error!(%error, "snapcast: FIFO reopen failed; writer stopping");
                        return;
                    }
                }
            }
            for frame in scratch.chunks_exact(4) {
                let l = i16::from_le_bytes([frame[0], frame[1]]) as f64 / 32768.0;
                let r = i16::from_le_bytes([frame[2], frame[3]]) as f64 / 32768.0;
                meter.push(l, r);
            }
            meter_frames += end - start;
            if meter_frames >= loaded.track.sample_rate as usize / 20 {
                if let Some(levels) = meter.take() {
                    tap.publish(levels);
                }
                meter_frames = 0;
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
            let generation = current
                .as_ref()
                .map(|loaded| loaded.generation)
                .unwrap_or_default();
            if let Some((track, gain)) = next.take() {
                if let Some(eq) = eq.as_mut() {
                    eq.reset();
                }
                meter.reset();
                meter_frames = 0;
                tap.clear();
                current = Some(Loaded {
                    track,
                    cursor_frames: 0,
                    gain,
                    generation,
                });
                let _ = events.send(WriterEvent::Advanced { generation });
            } else {
                current = None;
                let _ = events.send(WriterEvent::Drained { generation });
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
    meter: &mut StereoMeter,
    tap: &AudioTap,
) -> bool {
    match msg {
        WriterMsg::Load {
            track,
            start_frame,
            gain,
            generation,
        } => {
            meter.reset();
            tap.clear();
            let cursor_frames = start_frame.min(track.frames());
            *current = Some(Loaded {
                track,
                cursor_frames,
                gain,
                generation,
            });
            *next = None;
            if let Some(eq) = eq.as_mut() {
                eq.reset(); // don't bleed the previous track's filter state into the new one
            }
        }
        WriterMsg::Preload {
            track,
            gain,
            generation,
        } => {
            // A decode started for the previous current track can finish after a
            // replacement Load. Never let it seed that new output's next track.
            if current
                .as_ref()
                .is_some_and(|loaded| loaded.generation == generation)
            {
                *next = Some((track, gain));
            }
        }
        WriterMsg::SetPlaying(value) => {
            *playing = value;
            if !value {
                meter.reset();
                tap.clear();
            }
        }
        WriterMsg::SetVolume(percent) => *volume = (percent.min(100) as f32) / 100.0,
        WriterMsg::Seek { frame } => {
            meter.reset();
            tap.clear();
            if let Some(loaded) = current.as_mut() {
                loaded.cursor_frames = frame.min(loaded.track.frames());
            }
            if let Some(eq) = eq.as_mut() {
                eq.reset();
            }
        }
        WriterMsg::SetDsp { chain, revision } => {
            *eq = *chain;
            meter.reset();
            tap.clear();
            tap.applied_revision.store(revision, Ordering::Release);
        }
        WriterMsg::Stop => {
            meter.reset();
            tap.clear();
            *current = None;
            *next = None;
            *playing = false;
        }
        WriterMsg::Shutdown => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicata_core::dsp::{DspBand, DspProfile};

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "musicata-writer-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn prefetched_track_starts_with_clean_filter_and_meter_history() {
        let file = path("gapless");
        std::fs::write(&file, []).unwrap();
        let tap = Arc::new(AudioTap::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let profile = DspProfile {
            id: "test".into(),
            name: "Test".into(),
            preamp_db: 0.,
            bands: vec![DspBand {
                band_type: "peaking".into(),
                freq: 1000.,
                gain: 6.,
                q: 1.,
            }],
            kind: None,
            room_ir: None,
        };
        tx.send(WriterMsg::SetDsp {
            chain: Box::new(StereoEq::from_profile(&profile, 48000).unwrap()),
            revision: 1,
        })
        .unwrap();
        tx.send(WriterMsg::Load {
            track: Arc::new(DecodedTrack {
                samples: vec![16000; 200],
                sample_rate: 48000,
            }),
            start_frame: 0,
            gain: 1.,
            generation: 1,
        })
        .unwrap();
        tx.send(WriterMsg::Preload {
            track: Arc::new(DecodedTrack {
                samples: vec![0; 4800],
                sample_rate: 48000,
            }),
            gain: 1.,
            generation: 1,
        })
        .unwrap();
        tx.send(WriterMsg::SetPlaying(true)).unwrap();
        let writer_tap = tap.clone();
        let writer_file = file.clone();
        let writer = std::thread::spawn(move || run(writer_file, rx, events, writer_tap));
        loop {
            if matches!(received.blocking_recv(), Some(WriterEvent::Drained { .. })) {
                break;
            }
        }
        tx.send(WriterMsg::Shutdown).unwrap();
        writer.join().unwrap();
        let bytes = std::fs::read(&file).unwrap();
        std::fs::remove_file(file).unwrap();
        assert!(
            bytes[400..].iter().all(|byte| *byte == 0),
            "filter tail crossed the track boundary"
        );
        assert_eq!(
            tap.read().unwrap().1.peak_l,
            0.,
            "old track contaminated the new meter window"
        );
    }

    #[test]
    fn fifo_reopen_recovers_the_applied_revision_and_output() {
        use std::io::Read;
        let fifo = path("recovery");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let tap = Arc::new(AudioTap::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        tx.send(WriterMsg::SetDsp {
            chain: Box::new(None),
            revision: 9,
        })
        .unwrap();
        tx.send(WriterMsg::Load {
            track: Arc::new(DecodedTrack {
                samples: vec![16000; 96000],
                sample_rate: 48000,
            }),
            start_frame: 0,
            gain: 1.,
            generation: 1,
        })
        .unwrap();
        tx.send(WriterMsg::SetPlaying(true)).unwrap();
        let writer_file = fifo.clone();
        let writer_tap = tap.clone();
        let writer = std::thread::spawn(move || run(writer_file, rx, events, writer_tap));
        let mut reader = std::fs::File::open(&fifo).unwrap();
        reader.read_exact(&mut [0; 3840]).unwrap();
        // The original reader disappears; reopen finds a replacement stream at the same path.
        std::fs::remove_file(&fifo).unwrap();
        std::fs::write(&fifo, []).unwrap();
        drop(reader);
        assert!(matches!(
            received.blocking_recv(),
            Some(WriterEvent::Drained { .. })
        ));
        assert_eq!(tap.applied_revision.load(Ordering::Acquire), 9);
        assert!(tap.read().is_some());
        tx.send(WriterMsg::Shutdown).unwrap();
        writer.join().unwrap();
        assert!(!std::fs::read(&fifo).unwrap().is_empty());
        std::fs::remove_file(fifo).unwrap();
    }

    #[test]
    fn failed_fifo_write_clears_applied_revision_and_meter() {
        let tap = Arc::new(AudioTap::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        tx.send(WriterMsg::SetDsp {
            chain: Box::new(None),
            revision: 9,
        })
        .unwrap();
        tx.send(WriterMsg::Load {
            track: Arc::new(DecodedTrack {
                samples: vec![16000; 6000],
                sample_rate: 48000,
            }),
            start_frame: 0,
            gain: 1.,
            generation: 1,
        })
        .unwrap();
        tx.send(WriterMsg::SetPlaying(true)).unwrap();
        run(PathBuf::from("/dev/full"), rx, events, tap.clone());
        assert_eq!(tap.applied_revision.load(Ordering::Acquire), 0);
        assert!(tap.read().is_none());
    }
}
