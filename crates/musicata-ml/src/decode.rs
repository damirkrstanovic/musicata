//! Decode arbitrary audio bytes to **mono f32 at the model's sample rate** (16 kHz). The PANNs
//! model takes a raw waveform (the spectrogram is inside the ONNX graph), so this is the whole
//! preprocessing step. Mirrors the server's Snapcast decode loop (symphonia + rubato).

use std::io::Cursor;

use anyhow::{Result, anyhow};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decode `audio` (any supported format) to a mono f32 waveform at `target_rate` Hz.
pub fn decode_to_mono(audio: &[u8], target_rate: u32) -> Result<Vec<f32>> {
    let stream = MediaSourceStream::new(Box::new(Cursor::new(audio.to_vec())), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| anyhow!("probe: {error}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no decodable audio track"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| anyhow!("decoder: {error}"))?;

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut source_rate = 0u32;
    let mut channels = 0usize;
    let mut mono: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
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
                    channels = spec.channels.count();
                    if source_rate == 0 || channels == 0 {
                        return Err(anyhow!("decoded audio had no channels / sample rate"));
                    }
                    sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                }
                if let Some(buf) = sample_buf.as_mut() {
                    buf.copy_interleaved_ref(decoded);
                    downmix_mono(&mut mono, buf.samples(), channels);
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if mono.is_empty() {
        return Err(anyhow!("no audio decoded"));
    }
    if source_rate == target_rate {
        Ok(mono)
    } else {
        resample_mono(&mono, source_rate, target_rate)
    }
}

/// Append a packet's interleaved samples to `out`, downmixing to mono (average of channels).
fn downmix_mono(out: &mut Vec<f32>, interleaved: &[f32], channels: usize) {
    match channels {
        0 => {}
        1 => out.extend_from_slice(interleaved),
        n => {
            for frame in interleaved.chunks_exact(n) {
                out.push(frame.iter().sum::<f32>() / n as f32);
            }
        }
    }
}

/// Resample a mono f32 signal from `from` Hz to `to` Hz (whole clip in one call).
fn resample_mono(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    let frames_in = input.len();
    let mut resampler = Fft::<f32>::new(from as usize, to as usize, 1024, 2, 1, FixedSync::Input)
        .map_err(|error| anyhow!("resampler init: {error}"))?;
    let needed = resampler.process_all_needed_output_len(frames_in);
    let mut output = vec![0.0f32; needed];
    let in_adapter =
        InterleavedSlice::new(input, 1, frames_in).map_err(|error| anyhow!("input adapter: {error}"))?;
    let mut out_adapter = InterleavedSlice::new_mut(&mut output, 1, needed)
        .map_err(|error| anyhow!("output adapter: {error}"))?;
    let (_in, out_frames) = resampler
        .process_all_into_buffer(&in_adapter, &mut out_adapter, frames_in, None)
        .map_err(|error| anyhow!("resample: {error}"))?;
    output.truncate(out_frames);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short mono sine-tone PCM WAV at `sample_rate`.
    fn sine_wav(sample_rate: u32, seconds: u32) -> Vec<u8> {
        let frames = sample_rate * seconds;
        let data_len = frames * 2; // mono, 16-bit
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let t = n as f32 / sample_rate as f32;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            wav.extend_from_slice(&((sample * i16::MAX as f32) as i16).to_le_bytes());
        }
        wav
    }

    #[test]
    fn decodes_and_resamples_to_16k() {
        // A 1 s 44.1 kHz tone resamples to ~16000 mono samples.
        let wav = sine_wav(44_100, 1);
        let mono = decode_to_mono(&wav, 16_000).expect("decode");
        assert!(
            (mono.len() as i64 - 16_000).abs() < 400,
            "expected ~16000 samples, got {}",
            mono.len()
        );
        // Real signal, not silence.
        assert!(mono.iter().any(|&s| s.abs() > 0.1));
    }

    #[test]
    fn passthrough_when_already_16k() {
        let wav = sine_wav(16_000, 1);
        let mono = decode_to_mono(&wav, 16_000).expect("decode");
        assert!((mono.len() as i64 - 16_000).abs() < 50);
    }
}
