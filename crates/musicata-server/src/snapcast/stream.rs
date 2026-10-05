// SPDX-License-Identifier: AGPL-3.0-or-later
//! Incremental decoding for one Internet-radio stream. HLS playlists are not supported;
//! callers must supply a direct HTTP(S) audio stream.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::decode::CHANNELS;

const CHUNK_FRAMES: usize = 960;
const CHANNEL_CAPACITY: usize = 8;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// A live decoded stream at the Snapcast stream rate. Dropping it tells the blocking HTTP and
/// decoder worker to stop; a full output channel is never allowed to retain that worker.
pub(crate) struct LiveStream {
    pub(crate) chunks: mpsc::Receiver<Result<Vec<i16>, String>>,
    pub(crate) sample_rate: u32,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Begin fetching and decoding a direct radio stream. The caller receives PCM incrementally;
/// the worker reports HTTP, decoder, and EOF failures through `chunks`.
pub(crate) fn stream_radio(url: String, target_rate: u32) -> LiveStream {
    let (tx, chunks) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    thread::Builder::new()
        .name("musicata-radio-decode".to_string())
        .spawn(move || {
            if let Err(error) = decode_radio(&url, target_rate, &tx, &worker_cancelled) {
                if !worker_cancelled.load(Ordering::Acquire) {
                    let _ = send_chunk(&tx, Err(error), &worker_cancelled);
                }
            }
        })
        .expect("spawn radio decoder thread");
    LiveStream {
        chunks,
        sample_rate: target_rate,
        cancelled,
    }
}

fn decode_radio(
    url: &str,
    target_rate: u32,
    tx: &SyncSender<Result<Vec<i16>, String>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    if !is_http_url(url) {
        return Err("radio stream URL must use HTTP or HTTPS".to_string());
    }
    if target_rate == 0 {
        return Err("radio stream sample rate must be non-zero".to_string());
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(HTTP_TIMEOUT)
        .timeout_read(HTTP_TIMEOUT)
        .build();
    let response = agent
        .get(url)
        .set("Icy-MetaData", "0")
        .call()
        .map_err(|error| format!("radio HTTP request: {error}"))?;
    let content_type = response.content_type().to_string();
    let meta_interval = response
        .header("icy-metaint")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0);
    let source = IcyReader::new(response.into_reader(), meta_interval);
    let stream = MediaSourceStream::new(Box::new(ReadOnlySource::new(source)), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = extension_hint(url, &content_type) {
        hint.with_extension(&extension);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("radio probe: {error}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "radio stream has no decodable audio track".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| format!("radio decoder: {error}"))?;

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut source_rate = 0;
    let mut source_channels = 0;
    let mut resampler: Option<StreamResampler> = None;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let packet = format
            .next_packet()
            .map_err(|error| format!("radio stream ended: {error}"))?;
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let channels = spec.channels.count();
                if spec.rate == 0 || channels == 0 {
                    return Err("radio decoder produced no channels / sample rate".to_string());
                }
                if source_rate != spec.rate || source_channels != channels {
                    source_rate = spec.rate;
                    source_channels = channels;
                    sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                    resampler = if source_rate == target_rate {
                        None
                    } else {
                        Some(StreamResampler::new(source_rate, target_rate)?)
                    };
                }
                let Some(buf) = sample_buf.as_mut() else {
                    return Err("radio decoder buffer unavailable".to_string());
                };
                buf.copy_interleaved_ref(decoded);
                let stereo = to_stereo(buf.samples(), source_channels);
                if let Some(resampler) = resampler.as_mut() {
                    resampler.push(&stereo, tx, cancelled)?;
                } else {
                    send_samples(&stereo, tx, cancelled)?;
                }
            }
            // Streams occasionally carry a corrupt frame; retain the decoder state and continue.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(format!("radio decode: {error}")),
        }
    }
}

fn send_samples(
    samples: &[f32],
    tx: &SyncSender<Result<Vec<i16>, String>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    for frames in samples.chunks(CHUNK_FRAMES * CHANNELS) {
        if frames.len() < CHANNELS {
            continue;
        }
        let length = frames.len() / CHANNELS * CHANNELS;
        let chunk = frames[..length].iter().map(to_i16).collect();
        if !send_chunk(tx, Ok(chunk), cancelled) {
            return Ok(());
        }
    }
    Ok(())
}

fn send_chunk(
    tx: &SyncSender<Result<Vec<i16>, String>>,
    item: Result<Vec<i16>, String>,
    cancelled: &AtomicBool,
) -> bool {
    let mut item = item;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(value)) => {
                item = value;
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn extension_hint(url: &str, content_type: &str) -> Option<String> {
    let mime_hint = match content_type.to_ascii_lowercase().as_str() {
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/aac" | "audio/aacp" => Some("aac"),
        "audio/ogg" | "application/ogg" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    };
    mime_hint.map(str::to_string).or_else(|| {
        url.split(['?', '#'])
            .next()
            .and_then(|path| path.rsplit_once('.'))
            .map(|(_, extension)| extension.to_ascii_lowercase())
    })
}

fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(samples.len().max(CHANNELS) / channels.max(1) * CHANNELS);
    match channels {
        1 => {
            for &sample in samples {
                stereo.extend([sample, sample]);
            }
        }
        2 => stereo.extend_from_slice(samples),
        channels => {
            for frame in samples.chunks_exact(channels) {
                stereo.extend([frame[0], frame[1]]);
            }
        }
    }
    stereo
}

fn to_i16(sample: &f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

struct StreamResampler {
    inner: Fft<f32>,
    pending: Vec<f32>,
    trim: usize,
}

impl StreamResampler {
    fn new(source_rate: u32, target_rate: u32) -> Result<Self, String> {
        let inner = Fft::<f32>::new(
            source_rate as usize,
            target_rate as usize,
            1024,
            2,
            CHANNELS,
            FixedSync::Input,
        )
        .map_err(|error| format!("radio resampler init: {error}"))?;
        let trim = inner.output_delay();
        Ok(Self {
            inner,
            pending: Vec::new(),
            trim,
        })
    }

    fn push(
        &mut self,
        samples: &[f32],
        tx: &SyncSender<Result<Vec<i16>, String>>,
        cancelled: &AtomicBool,
    ) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        let needed = self.inner.input_frames_next();
        while self.pending.len() / CHANNELS >= needed {
            let input: Vec<f32> = self.pending.drain(..needed * CHANNELS).collect();
            let output_frames = self.inner.output_frames_max();
            let mut output = vec![0.0; output_frames * CHANNELS];
            let input = InterleavedSlice::new(&input, CHANNELS, needed)
                .map_err(|error| format!("radio resampler input: {error}"))?;
            let mut output_adapter =
                InterleavedSlice::new_mut(&mut output, CHANNELS, output_frames)
                    .map_err(|error| format!("radio resampler output: {error}"))?;
            let (_, frames) = self
                .inner
                .process_into_buffer(&input, &mut output_adapter, None)
                .map_err(|error| format!("radio resample: {error}"))?;
            let start = self.trim.min(frames);
            self.trim -= start;
            send_samples(&output[start * CHANNELS..frames * CHANNELS], tx, cancelled)?;
        }
        Ok(())
    }
}

/// Removes ICY metadata blocks from an HTTP body before Symphonia sees it.
struct IcyReader<R> {
    inner: R,
    interval: Option<usize>,
    audio_until_metadata: usize,
}

impl<R> IcyReader<R> {
    fn new(inner: R, interval: Option<usize>) -> Self {
        Self {
            inner,
            interval,
            audio_until_metadata: interval.unwrap_or(usize::MAX),
        }
    }
}

impl<R: Read> Read for IcyReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let Some(interval) = self.interval else {
            return self.inner.read(out);
        };
        let mut written = 0;
        while written < out.len() {
            if self.audio_until_metadata == 0 {
                let mut length = [0u8; 1];
                match self.inner.read_exact(&mut length) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                        return Ok(written);
                    }
                    Err(error) => return Err(error),
                }
                let metadata_len = length[0] as usize * 16;
                let mut discard = vec![0; metadata_len];
                self.inner.read_exact(&mut discard)?;
                self.audio_until_metadata = interval;
                continue;
            }
            let available = (out.len() - written).min(self.audio_until_metadata);
            let count = self.inner.read(&mut out[written..written + available])?;
            if count == 0 {
                break;
            }
            written += count;
            self.audio_until_metadata -= count;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn wav(sample_rate: u32, frames: u32) -> Vec<u8> {
        let data_len = frames * 2 * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let sample = ((frame % 100) as i16 - 50) * 500;
            bytes.extend_from_slice(&sample.to_le_bytes());
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn http_audio(body: Vec<u8>, headers: &str) -> String {
        http_encoded(body, headers, "audio/wav", "wav")
    }

    fn http_encoded(
        body: Vec<u8>,
        headers: &str,
        content_type: &'static str,
        extension: &str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let headers = headers.to_string();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{}\r\n",
                body.len(),
                headers
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        format!("http://{address}/station.{extension}")
    }

    #[test]
    fn rejects_non_http_urls_without_starting_a_worker() {
        let stream = stream_radio("file:///music/radio.mp3".to_string(), 48_000);
        assert_eq!(stream.sample_rate, 48_000);
        assert!(stream.chunks.recv().unwrap().unwrap_err().contains("HTTP"));
    }

    #[test]
    fn decodes_http_audio_and_resamples_to_stereo_chunks() {
        let url = http_audio(wav(44_100, 4_096), "");
        let stream = stream_radio(url, 48_000);
        let mut frames = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match stream.chunks.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(chunk)) => {
                    assert!(chunk.len() <= CHUNK_FRAMES * CHANNELS);
                    assert_eq!(chunk.len() % CHANNELS, 0);
                    frames += chunk.len() / CHANNELS;
                }
                Ok(Err(error)) => {
                    assert!(error.contains("ended"));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => panic!("unexpected stream close: {error}"),
            }
        }
        assert!(frames > 2_500, "decoded too little audio: {frames} frames");
    }

    #[test]
    fn decodes_compressed_mp3_from_a_non_seekable_http_stream() {
        let audio =
            include_bytes!("../../../../testdata-fixture/The Meridian/Neon Hours/01 Track.mp3");
        let stream = stream_radio(
            http_encoded(audio.to_vec(), "", "audio/mpeg", "mp3"),
            48_000,
        );
        let mut frames = 0;
        loop {
            match stream.chunks.recv_timeout(Duration::from_secs(5)).unwrap() {
                Ok(chunk) => {
                    assert!(chunk.len() <= CHUNK_FRAMES * CHANNELS);
                    frames += chunk.len() / CHANNELS;
                }
                Err(error) => {
                    assert!(error.contains("ended"), "{error}");
                    break;
                }
            }
        }
        assert!(
            frames > 48_000,
            "MP3 broadcast produced too little PCM: {frames}"
        );
    }

    #[test]
    fn decodes_adts_aac_from_http() {
        let audio = include_bytes!("testdata/radio.aac");
        let stream = stream_radio(http_encoded(audio.to_vec(), "", "audio/aac", "aac"), 48_000);
        let mut frames = 0;
        loop {
            match stream.chunks.recv_timeout(Duration::from_secs(5)).unwrap() {
                Ok(chunk) => frames += chunk.len() / CHANNELS,
                Err(error) => {
                    assert!(error.contains("ended"), "{error}");
                    break;
                }
            }
        }
        assert!(
            frames > 40_000,
            "AAC broadcast produced too little PCM: {frames}"
        );
    }

    #[test]
    fn drop_cancels_a_full_output_channel() {
        let url = http_audio(wav(48_000, 96_000), "");
        let stream = stream_radio(url, 48_000);
        let cancelled = Arc::clone(&stream.cancelled);
        thread::sleep(Duration::from_millis(100));
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(&cancelled) > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(
            Arc::strong_count(&cancelled),
            1,
            "cancelled decoder worker did not exit"
        );
    }

    #[test]
    fn icy_reader_removes_metadata_blocks() {
        let reader = IcyReader::new(&b"ab\x01StreamTitle='x';cd\x00ef"[..], Some(2));
        let mut output = Vec::new();
        let mut reader = reader;
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"abcdef");
    }
}
