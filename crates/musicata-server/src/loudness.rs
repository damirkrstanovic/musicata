//! EBU R128 loudness analysis for volume leveling. `symphonia` decodes the whole track and
//! `ebur128` measures its integrated loudness (LUFS) + true-peak (dBTP); the playback path
//! turns those into a per-track gain so quiet and loud tracks play at the same level. Pure,
//! synchronous, CPU-heavy (a full decode per track) — the caller runs it on `spawn_blocking`
//! from the `loudness_loop`, and the result is cached in `track_loudness` (analyzed once).

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use ebur128::{EbuR128, Mode};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// A `MediaSource` that stops serving bytes once a wall-clock deadline passes. Format probing
/// (which can scan a whole malformed file before the decode loop even starts) and every
/// `next_packet` read flow through this, so the deadline bounds *all* the decode work — not just
/// the loop. After the deadline, reads return `TimedOut`, so `symphonia` aborts with an error.
struct DeadlineSource {
    inner: Cursor<Vec<u8>>,
    deadline: Instant,
}

impl Read for DeadlineSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "loudness analysis budget exceeded",
            ));
        }
        self.inner.read(buf)
    }
}

impl Seek for DeadlineSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for DeadlineSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.inner.get_ref().len() as u64)
    }
}

/// Floor for silent / below-gate tracks: `loudness_global` returns -inf when nothing crosses
/// the absolute gate, which would produce an absurd leveling boost — clamp it.
const SILENCE_LUFS: f64 = -70.0;

/// Decode an audio file's bytes in full and measure (integrated LUFS, true-peak dBTP).
/// `extension` (e.g. `flac`) hints the demuxer. Synchronous + CPU-heavy.
///
/// `max_wall` bounds the decode: a malformed track can decode into an endless run of tiny
/// garbage frames (or simply be enormous), pinning a CPU core for time proportional to its
/// bytes — this runs in `spawn_blocking`, which can't be cancelled from outside, so the bound
/// must be internal. Exceeding the budget returns `Err`; the caller marks the track
/// un-measurable so it isn't retried, rather than the loudness loop spinning on it.
pub fn analyze_loudness(
    audio: &[u8],
    extension: &str,
    max_wall: Duration,
) -> Result<(f64, f64), String> {
    let deadline = Instant::now() + max_wall;
    let source = DeadlineSource {
        inner: Cursor::new(audio.to_vec()),
        deadline,
    };
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("probe: {error}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no decodable audio track".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| format!("decoder: {error}"))?;

    let mut analyzer: Option<EbuR128> = None;
    let mut sample_buf: Option<SampleBuffer<i16>> = None;
    let mut channels = 0u32;

    loop {
        if Instant::now() >= deadline {
            return Err(format!("analysis exceeded the {max_wall:?} budget"));
        }
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // End of stream (or an unrecoverable read) — stop with what we have.
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if analyzer.is_none() {
                    let spec = *decoded.spec();
                    channels = spec.channels.count() as u32;
                    if channels == 0 || spec.rate == 0 {
                        return Err("decoded audio had no channels / sample rate".to_string());
                    }
                    let ebu = EbuR128::new(channels, spec.rate, Mode::I | Mode::TRUE_PEAK)
                        .map_err(|error| format!("ebur128 init: {error}"))?;
                    analyzer = Some(ebu);
                    sample_buf = Some(SampleBuffer::<i16>::new(decoded.capacity() as u64, spec));
                }
                if let (Some(ebu), Some(buf)) = (analyzer.as_mut(), sample_buf.as_mut()) {
                    buf.copy_interleaved_ref(decoded);
                    ebu.add_frames_i16(buf.samples())
                        .map_err(|error| format!("ebur128 add: {error}"))?;
                }
            }
            // A single corrupt packet shouldn't abort the whole decode.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    // A deadline hit while decoding leaves the loop via a read error (`break`); surface it as
    // un-measurable rather than reporting a reading from a truncated, possibly-garbage decode.
    if Instant::now() >= deadline {
        return Err(format!("analysis exceeded the {max_wall:?} budget"));
    }
    let ebu = analyzer.ok_or_else(|| "no audio decoded".to_string())?;
    let lufs = ebu
        .loudness_global()
        .map_err(|error| format!("loudness: {error}"))?;
    let lufs = if lufs.is_finite() { lufs } else { SILENCE_LUFS };

    // `true_peak` returns a linear amplitude (1.0 = 0 dBFS, may exceed 1.0); take the max
    // across channels and convert to dBTP.
    let peak = (0..channels)
        .filter_map(|ch| ebu.true_peak(ch).ok())
        .fold(0.0f64, f64::max);
    let true_peak_dbtp = if peak > 0.0 { 20.0 * peak.log10() } else { -120.0 };

    Ok((lufs, true_peak_dbtp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short sine-tone PCM WAV so the analysis produces a real (non-silent) reading.
    fn sine_wav(seconds: u32, amplitude: f64) -> Vec<u8> {
        let sample_rate: u32 = 48000;
        let channels: u16 = 2;
        let frames = sample_rate * seconds;
        let mut data = Vec::with_capacity((frames * channels as u32 * 2) as usize);
        for n in 0..frames {
            let t = n as f64 / sample_rate as f64;
            let v = (amplitude * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * i16::MAX as f64)
                as i16;
            for _ in 0..channels {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        let data_len = data.len() as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend(data);
        wav
    }

    /// A normal track well under the budget analyzes to completion.
    const TEST_BUDGET: Duration = Duration::from_secs(30);

    #[test]
    fn measures_a_sine_tone() {
        let (lufs, peak) =
            analyze_loudness(&sine_wav(4, 0.5), "wav", TEST_BUDGET).expect("analyze");
        // A half-scale 440 Hz tone sits well within a sane loudness range and below 0 dBFS.
        assert!(lufs > -30.0 && lufs < 0.0, "plausible LUFS, got {lufs}");
        assert!(peak < 0.0 && peak > -12.0, "true-peak below 0 dBFS, got {peak}");
    }

    #[test]
    fn louder_tone_measures_higher() {
        let (quiet, _) = analyze_loudness(&sine_wav(4, 0.2), "wav", TEST_BUDGET).expect("quiet");
        let (loud, _) = analyze_loudness(&sine_wav(4, 0.8), "wav", TEST_BUDGET).expect("loud");
        assert!(loud > quiet + 3.0, "louder tone is measurably louder: {quiet} -> {loud}");
    }

    /// A malformed track that decodes into a dense run of garbage frames used to pin a CPU core
    /// for time proportional to its byte size (no cap) — the "spin". The wall-clock budget must
    /// abandon it. This is a bounded-termination guard, not a latency-budget assertion: 64 MB of
    /// MP3-sync garbage takes ~6 s uncapped in the debug test profile, so a short budget must
    /// return an error in a small fraction of that.
    #[test]
    fn aborts_a_pathological_decode_within_the_budget() {
        let garbage = [0xFFu8, 0xFB, 0x90, 0x00].repeat(64 * 262_144); // 64 MiB
        let started = Instant::now();
        let result = analyze_loudness(&garbage, "mp3", Duration::from_millis(300));
        let elapsed = started.elapsed();
        assert!(result.is_err(), "a pathological decode must not yield a reading");
        assert!(
            elapsed < Duration::from_secs(3),
            "budget must abandon the decode promptly, took {elapsed:?}"
        );
    }
}
