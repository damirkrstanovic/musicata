// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded, cancellable library decoding for the Snapcast writer.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_NULL, DecoderOptions},
    errors::Error as DecodeError,
    formats::{FormatOptions, SeekMode, SeekTo},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};

const CHUNK_FRAMES: usize = 960;
// Two active/preloaded decoders plus limited cancellation overlap. Blocking source reads
// cannot create unbounded threads when a listener repeatedly skips a slow source.
static DECODER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(crate) struct LibraryStream {
    pub chunks: Mutex<mpsc::Receiver<Result<Vec<i16>, String>>>,
    pub sample_rate: u32,
    cancelled: Arc<AtomicBool>,
}
impl Drop for LibraryStream {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) async fn stream_track(
    database: &musicata_storage::Database,
    providers: &Arc<tokio::sync::RwLock<crate::providers::ProviderRegistry>>,
    id: &str,
    rate: u32,
    start_seconds: f64,
) -> Result<Arc<LibraryStream>, String> {
    let track = database
        .track(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Unknown track {id}"))?;
    #[cfg(feature = "provider-smb")]
    {
        let smb = {
            let providers = providers.read().await;
            match providers.get(&track.provider.provider_id) {
                Some(crate::providers::ProviderHandle::Smb(smb)) => Some(smb.clone()),
                _ => None,
            }
        };
        if let Some(smb) = smb {
            let length = track
                .file_size_bytes
                .ok_or("SMB track size unavailable; rescan the source")?;
            return stream_source(
                Box::new(SmbSource {
                    provider: smb,
                    item: track.provider.item_id,
                    length,
                    offset: 0,
                    cache: Vec::new(),
                    cache_start: 0,
                    runtime: tokio::runtime::Handle::current(),
                }),
                track.extension,
                rate,
                start_seconds,
            )
            .await;
        }
    }
    let file = tokio::fs::File::open(track.path)
        .await
        .map_err(|e| e.to_string())?
        .into_std()
        .await;
    stream_source(Box::new(file), track.extension, rate, start_seconds).await
}

pub(crate) async fn stream_source(
    source: Box<dyn MediaSource>,
    extension: String,
    rate: u32,
    start: f64,
) -> Result<Arc<LibraryStream>, String> {
    if rate == 0 || !start.is_finite() || start < 0.0 {
        return Err("Invalid decoder rate or position".into());
    }
    let permit = DECODER_SLOTS.acquire().await.map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::sync_channel(8);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Err(error) = decode(source, &extension, rate, start, &tx, &worker_cancelled) {
            send(&tx, Err(error), &worker_cancelled);
        }
    });
    Ok(Arc::new(LibraryStream {
        chunks: Mutex::new(rx),
        sample_rate: rate,
        cancelled,
    }))
}

fn send(
    tx: &mpsc::SyncSender<Result<Vec<i16>, String>>,
    mut value: Result<Vec<i16>, String>,
    cancelled: &AtomicBool,
) -> bool {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        match tx.try_send(value) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(v)) => {
                value = v;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn emit(
    samples: &[f32],
    skip: &mut usize,
    pending: &mut Vec<i16>,
    tx: &mpsc::SyncSender<Result<Vec<i16>, String>>,
    cancelled: &AtomicBool,
) -> bool {
    let skipped = (*skip).min(samples.len() / 2);
    *skip -= skipped;
    for sample in &samples[skipped * 2..] {
        pending.push((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        if pending.len() == CHUNK_FRAMES * 2 {
            let pcm = std::mem::replace(pending, Vec::with_capacity(CHUNK_FRAMES * 2));
            if !send(tx, Ok(pcm), cancelled) {
                return false;
            }
        }
    }
    true
}

fn decode(
    source: Box<dyn MediaSource>,
    extension: &str,
    rate: u32,
    start: f64,
    tx: &mpsc::SyncSender<Result<Vec<i16>, String>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut first_pcm_timing = Some(crate::diagnostics::Timing::new(
        crate::diagnostics::Operation::Decode,
    ));
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            MediaSourceStream::new(source, Default::default()),
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("No decodable audio track")?;
    let id = track.id;
    let time_base = track.codec_params.time_base;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let mut skip = 0;
    if start > 0.0 {
        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: start.into(),
                    track_id: Some(id),
                },
            )
            .map_err(|e| format!("Seek failed: {e}"))?;
        if let Some(base) = time_base {
            let delta = base.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts));
            skip = ((delta.seconds as f64 + delta.frac) * rate as f64).round() as usize;
        }
        decoder.reset();
    }
    let mut converter: Option<Converter> = None;
    // Coalesce decoder/resampler packet tails. Short intermediate writes waste FIFO
    // slots and can starve Snapserver even when average decoding throughput is ample.
    let mut pending = Vec::with_capacity(CHUNK_FRAMES * 2);
    while !cancelled.load(Ordering::Acquire) {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(DecodeError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id() != id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(DecodeError::DecodeError(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count();
        if channels == 0 || spec.rate == 0 {
            return Err("Invalid audio format".into());
        }
        if converter.as_ref().is_some_and(|c| c.from != spec.rate) {
            return Err("Sample rate changed inside a library track".into());
        }
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        let stereo: Vec<f32> = samples
            .samples()
            .chunks_exact(channels)
            .flat_map(|frame| [frame[0], frame[if channels == 1 { 0 } else { 1 }]])
            .collect();
        first_pcm_timing.take();
        if spec.rate == rate {
            if !emit(&stereo, &mut skip, &mut pending, tx, cancelled) {
                return Ok(());
            }
        } else {
            if converter.is_none() {
                converter = Some(Converter::new(spec.rate, rate)?);
            }
            for output in converter.as_mut().unwrap().push(&stereo, false)? {
                if !emit(&output, &mut skip, &mut pending, tx, cancelled) {
                    return Ok(());
                }
            }
        }
    }
    if let Some(converter) = converter.as_mut() {
        for output in converter.push(&[], true)? {
            if !emit(&output, &mut skip, &mut pending, tx, cancelled) {
                break;
            }
        }
    }
    if !pending.is_empty() {
        send(tx, Ok(pending), cancelled);
    }
    Ok(())
}

struct Converter {
    inner: Fft<f32>,
    pending: Vec<f32>,
    trim: usize,
    from: u32,
    to: u32,
    frames_in: usize,
    frames_out: usize,
}
impl Converter {
    fn new(from: u32, to: u32) -> Result<Self, String> {
        let inner = Fft::<f32>::new(from as usize, to as usize, 1024, 2, 2, FixedSync::Input)
            .map_err(|e| e.to_string())?;
        let trim = inner.output_delay();
        Ok(Self {
            inner,
            pending: Vec::new(),
            trim,
            from,
            to,
            frames_in: 0,
            frames_out: 0,
        })
    }
    fn push(&mut self, samples: &[f32], finish: bool) -> Result<Vec<Vec<f32>>, String> {
        self.frames_in += samples.len() / 2;
        self.pending.extend_from_slice(samples);
        let target = (self.frames_in as f64 * self.to as f64 / self.from as f64).round() as usize;
        let mut chunks = Vec::new();
        loop {
            let needed = self.inner.input_frames_next();
            if finish {
                if self.frames_out >= target {
                    break;
                }
                self.pending.resize(self.pending.len().max(needed * 2), 0.0);
            } else if self.pending.len() < needed * 2 {
                break;
            }
            let input: Vec<f32> = self.pending.drain(..needed * 2).collect();
            let capacity = self.inner.output_frames_max();
            let mut output = vec![0.0; capacity * 2];
            let input = InterleavedSlice::new(&input, 2, needed).map_err(|e| e.to_string())?;
            let mut out =
                InterleavedSlice::new_mut(&mut output, 2, capacity).map_err(|e| e.to_string())?;
            let (_, frames) = self
                .inner
                .process_into_buffer(&input, &mut out, None)
                .map_err(|e| e.to_string())?;
            let start = self.trim.min(frames);
            self.trim -= start;
            let end = if finish {
                start + (frames - start).min(target.saturating_sub(self.frames_out))
            } else {
                frames
            };
            self.frames_out += end - start;
            chunks.push(output[start * 2..end * 2].to_vec());
        }
        Ok(chunks)
    }
}

#[cfg(feature = "provider-smb")]
struct SmbSource {
    provider: Arc<crate::smb::SmbProvider>,
    item: String,
    length: u64,
    offset: u64,
    cache: Vec<u8>,
    cache_start: u64,
    runtime: tokio::runtime::Handle,
}
#[cfg(feature = "provider-smb")]
impl Read for SmbSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        use futures::TryStreamExt;
        if out.is_empty() || self.offset >= self.length {
            return Ok(0);
        }
        if self.offset < self.cache_start
            || self.offset >= self.cache_start + self.cache.len() as u64
        {
            let end = (self.offset + 256 * 1024 - 1).min(self.length - 1);
            self.cache = self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    self.provider
                        .read_range_stream(&self.item, self.offset, end)
                        .await
                        .map_err(io::Error::other)?
                        .try_fold(Vec::new(), |mut bytes, chunk| async move {
                            bytes.extend(chunk);
                            Ok(bytes)
                        })
                        .await
                })
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SMB audio read timed out"))?
            })?;
            self.cache_start = self.offset;
        }
        let start = (self.offset - self.cache_start) as usize;
        let n = out.len().min(self.cache.len().saturating_sub(start));
        out[..n].copy_from_slice(&self.cache[start..start + n]);
        self.offset += n as u64;
        Ok(n)
    }
}
#[cfg(feature = "provider-smb")]
impl Seek for SmbSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let value = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.offset as i128 + n as i128,
            SeekFrom::End(n) => self.length as i128 + n as i128,
        };
        if value < 0 || value > u64::MAX as i128 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"));
        }
        self.offset = value as u64;
        Ok(self.offset)
    }
}
#[cfg(feature = "provider-smb")]
impl MediaSource for SmbSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::atomic::AtomicUsize;

    fn wav(frames: usize, rate: u32) -> Vec<u8> {
        let bytes = (frames * 4) as u32;
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((bytes + 36).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * 4).to_le_bytes());
        out.extend(4u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend(bytes.to_le_bytes());
        for _ in 0..frames {
            out.extend(4000i16.to_le_bytes());
            out.extend((-2000i16).to_le_bytes());
        }
        out
    }

    struct Counted {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for Counted {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(bytes)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }
    impl MediaSource for Counted {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.inner.get_ref().len() as u64)
        }
    }

    #[tokio::test]
    async fn first_pcm_does_not_require_full_read_and_cancel_stops_backpressure() {
        let read = Arc::new(AtomicUsize::new(0));
        let bytes = wav(480_000, 48_000);
        let size = bytes.len();
        let stream = stream_source(
            Box::new(Counted {
                inner: Cursor::new(bytes),
                read: read.clone(),
            }),
            "wav".into(),
            48_000,
            0.0,
        )
        .await
        .unwrap();
        let first = stream
            .chunks
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(!first.is_empty());
        assert!(first.len() <= 1920);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            read.load(Ordering::Relaxed) < size / 2,
            "read entire track before playback"
        );
        let cancelled = stream.cancelled.clone();
        drop(stream);
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn finite_resampling_flushes_tail_and_seek_starts_at_requested_time() {
        for (rate, target, seek) in [(44_100, 48_000, 0.0), (48_000, 48_000, 0.5)] {
            let stream = stream_source(
                Box::new(Cursor::new(wav(rate as usize, rate))),
                "wav".into(),
                target,
                seek,
            )
            .await
            .unwrap();
            let mut count = 0;
            let mut short_tail = false;
            loop {
                match stream
                    .chunks
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                {
                    Ok(Ok(chunk)) => {
                        assert!(!short_tail, "short PCM chunks must only occur at EOF");
                        short_tail = chunk.len() < CHUNK_FRAMES * 2;
                        count += chunk.len() / 2;
                    }
                    Ok(Err(error)) => panic!("{error}"),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(error) => panic!("{error}"),
                }
            }
            let expected = ((1.0 - seek) * target as f64).round() as usize;
            assert!(
                count.abs_diff(expected) <= 1,
                "{rate} → {target}: {count} != {expected}"
            );
        }
    }
}
