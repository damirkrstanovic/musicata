// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server-side decode of a library track to the fixed PCM format snapserver consumes.
//!
//! Snapcast broadcasts one continuous PCM stream per zone, so — unlike every other
//! Musicata playback path, where the endpoint fetches a per-track URL and decodes it
//! itself — the server must decode here. This mirrors the `symphonia` loop in
//! `loudness.rs`/`fingerprint.rs`, but full-length and to `f32`: we downmix to stereo
//! and `rubato` resamples to the one fixed stream rate (the library is mixed
//! 44.1/48/96 kHz; snapserver runs at a single rate). Output is interleaved `i16`,
//! ready to write to the FIFO. Pure + blocking — call it from `spawn_blocking`.

use std::io::Cursor;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// The stream is always stereo — it matches snapserver's `sampleformat` channel count.
pub const CHANNELS: usize = 2;

/// A fully decoded track: interleaved L,R,L,R… `i16` at the target rate.
pub struct DecodedTrack {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl DecodedTrack {
    /// Number of stereo frames (one frame = one L + one R sample).
    pub fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }
}

/// Decode a complete encoded file to interleaved stereo `i16` at `target_rate`.
pub fn decode_track(
    audio: &[u8],
    extension: &str,
    target_rate: u32,
) -> Result<DecodedTrack, String> {
    let source = Cursor::new(audio.to_vec());
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

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut source_rate = 0u32;
    let mut source_channels = 0usize;
    // Interleaved stereo f32 at the source rate, accumulated across packets.
    let mut stereo: Vec<f32> = Vec::new();

    loop {
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
                if sample_buf.is_none() {
                    let spec = *decoded.spec();
                    source_rate = spec.rate;
                    source_channels = spec.channels.count();
                    if source_rate == 0 || source_channels == 0 {
                        return Err("decoded audio had no channels / sample rate".to_string());
                    }
                    sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                }
                if let Some(buf) = sample_buf.as_mut() {
                    buf.copy_interleaved_ref(decoded);
                    append_stereo(&mut stereo, buf.samples(), source_channels);
                }
            }
            // A single corrupt packet shouldn't abort the whole decode.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if stereo.is_empty() {
        return Err("no audio decoded".to_string());
    }

    let samples = if source_rate == target_rate {
        stereo.iter().map(to_i16).collect()
    } else {
        resample_stereo(&stereo, source_rate, target_rate)?
            .iter()
            .map(to_i16)
            .collect()
    };

    Ok(DecodedTrack {
        samples,
        sample_rate: target_rate,
    })
}

/// Append a packet's interleaved samples to `out` as stereo, downmixing/upmixing as
/// needed: mono is duplicated to both channels; >2-channel takes the front L/R pair.
fn append_stereo(out: &mut Vec<f32>, interleaved: &[f32], channels: usize) {
    match channels {
        0 => {}
        1 => {
            for &sample in interleaved {
                out.push(sample);
                out.push(sample);
            }
        }
        2 => out.extend_from_slice(interleaved),
        n => {
            for frame in interleaved.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}

/// Resample interleaved stereo f32 from `from` Hz to `to` Hz with rubato's FFT
/// resampler, processing the whole clip in one call (initial-delay silence trimmed).
fn resample_stereo(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>, String> {
    let frames_in = input.len() / CHANNELS;
    // Fixed-input FFT resampler, ~1024-frame chunks split into 2 sub-chunks (rubato's
    // suggested ~100–1000-frame sub-chunk target).
    let mut resampler = Fft::<f32>::new(
        from as usize,
        to as usize,
        1024,
        2,
        CHANNELS,
        FixedSync::Input,
    )
    .map_err(|error| format!("resampler init: {error}"))?;
    let needed = resampler.process_all_needed_output_len(frames_in);
    let mut output = vec![0.0f32; needed * CHANNELS];
    let in_adapter = InterleavedSlice::new(input, CHANNELS, frames_in)
        .map_err(|error| format!("input adapter: {error}"))?;
    let mut out_adapter = InterleavedSlice::new_mut(&mut output, CHANNELS, needed)
        .map_err(|error| format!("output adapter: {error}"))?;
    let (_in_frames, out_frames) = resampler
        .process_all_into_buffer(&in_adapter, &mut out_adapter, frames_in, None)
        .map_err(|error| format!("resample: {error}"))?;
    output.truncate(out_frames * CHANNELS);
    Ok(output)
}

/// Quantize a normalized f32 sample (`-1.0..=1.0`) to `i16`, clamping out-of-range peaks.
fn to_i16(sample: &f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short sine-tone PCM WAV at `sample_rate`, so decode produces real samples.
    fn sine_wav(sample_rate: u32, seconds: u32) -> Vec<u8> {
        let channels: u16 = 2;
        let frames = sample_rate * seconds;
        let data_len = frames * channels as u32 * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        let byte_rate = sample_rate * channels as u32 * 2;
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let t = n as f64 / sample_rate as f64;
            let v = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * i16::MAX as f64) as i16;
            for _ in 0..channels {
                wav.extend_from_slice(&v.to_le_bytes());
            }
        }
        wav
    }

    #[test]
    fn decodes_matching_rate_without_resample() {
        let wav = sine_wav(48_000, 1);
        let decoded = decode_track(&wav, "wav", 48_000).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        // ~1 second of stereo frames (allow a small codec-boundary slack).
        assert!((decoded.frames() as i64 - 48_000).abs() < 2_048);
    }

    #[test]
    fn resamples_to_target_rate() {
        let wav = sine_wav(44_100, 1);
        let decoded = decode_track(&wav, "wav", 48_000).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        // 1 s of 44.1k upsampled to 48k ≈ 48_000 frames (resampler delay trims to ~exact).
        assert!((decoded.frames() as i64 - 48_000).abs() < 4_096);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_track(b"not audio", "wav", 48_000).is_err());
    }
}
