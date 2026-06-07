//! EBU R128 loudness analysis for volume leveling. `symphonia` decodes the whole track and
//! `ebur128` measures its integrated loudness (LUFS) + true-peak (dBTP); the playback path
//! turns those into a per-track gain so quiet and loud tracks play at the same level. Pure,
//! synchronous, CPU-heavy (a full decode per track) — the caller runs it on `spawn_blocking`
//! from the `loudness_loop`, and the result is cached in `track_loudness` (analyzed once).

use std::io::Cursor;

use ebur128::{EbuR128, Mode};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Floor for silent / below-gate tracks: `loudness_global` returns -inf when nothing crosses
/// the absolute gate, which would produce an absurd leveling boost — clamp it.
const SILENCE_LUFS: f64 = -70.0;

/// Decode an audio file's bytes in full and measure (integrated LUFS, true-peak dBTP).
/// `extension` (e.g. `flac`) hints the demuxer. Synchronous + CPU-heavy.
pub fn analyze_loudness(audio: &[u8], extension: &str) -> Result<(f64, f64), String> {
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

    let mut analyzer: Option<EbuR128> = None;
    let mut sample_buf: Option<SampleBuffer<i16>> = None;
    let mut channels = 0u32;

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

    let ebu = analyzer.ok_or_else(|| "no audio decoded".to_string())?;
    let lufs = ebu
        .loudness_global()
        .map_err(|error| format!("loudness: {error}"))?;
    let lufs = if lufs.is_finite() { lufs } else { SILENCE_LUFS };

    // `true_peak` returns a linear amplitude (1.0 = 0 dBFS, may exceed 1.0); take the max
    // across channels and convert to dBTP.
    let mut peak = 0.0f64;
    for ch in 0..channels {
        if let Ok(p) = ebu.true_peak(ch) {
            if p > peak {
                peak = p;
            }
        }
    }
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

    #[test]
    fn measures_a_sine_tone() {
        let (lufs, peak) = analyze_loudness(&sine_wav(4, 0.5), "wav").expect("analyze");
        // A half-scale 440 Hz tone sits well within a sane loudness range and below 0 dBFS.
        assert!(lufs > -30.0 && lufs < 0.0, "plausible LUFS, got {lufs}");
        assert!(peak < 0.0 && peak > -12.0, "true-peak below 0 dBFS, got {peak}");
    }

    #[test]
    fn louder_tone_measures_higher() {
        let (quiet, _) = analyze_loudness(&sine_wav(4, 0.2), "wav").expect("quiet");
        let (loud, _) = analyze_loudness(&sine_wav(4, 0.8), "wav").expect("loud");
        assert!(loud > quiet + 3.0, "louder tone is measurably louder: {quiet} -> {loud}");
    }
}
