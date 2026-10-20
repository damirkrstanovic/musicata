// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact, streaming identity evidence for an encoded audio payload.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use symphonia::core::codecs::{
    CODEC_TYPE_NULL, CODEC_TYPE_PCM_ALAW, CODEC_TYPE_PCM_F32BE, CODEC_TYPE_PCM_F32BE_PLANAR,
    CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_F32LE_PLANAR, CODEC_TYPE_PCM_F64BE,
    CODEC_TYPE_PCM_F64BE_PLANAR, CODEC_TYPE_PCM_F64LE, CODEC_TYPE_PCM_F64LE_PLANAR,
    CODEC_TYPE_PCM_MULAW, CODEC_TYPE_PCM_S8, CODEC_TYPE_PCM_S8_PLANAR, CODEC_TYPE_PCM_S16BE,
    CODEC_TYPE_PCM_S16BE_PLANAR, CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S16LE_PLANAR,
    CODEC_TYPE_PCM_S24BE, CODEC_TYPE_PCM_S24BE_PLANAR, CODEC_TYPE_PCM_S24LE,
    CODEC_TYPE_PCM_S24LE_PLANAR, CODEC_TYPE_PCM_S32BE, CODEC_TYPE_PCM_S32BE_PLANAR,
    CODEC_TYPE_PCM_S32LE, CODEC_TYPE_PCM_S32LE_PLANAR, CODEC_TYPE_PCM_U8, CODEC_TYPE_PCM_U8_PLANAR,
    CODEC_TYPE_PCM_U16BE, CODEC_TYPE_PCM_U16BE_PLANAR, CODEC_TYPE_PCM_U16LE,
    CODEC_TYPE_PCM_U16LE_PLANAR, CODEC_TYPE_PCM_U24BE, CODEC_TYPE_PCM_U24BE_PLANAR,
    CODEC_TYPE_PCM_U24LE, CODEC_TYPE_PCM_U24LE_PLANAR, CODEC_TYPE_PCM_U32BE,
    CODEC_TYPE_PCM_U32BE_PLANAR, CODEC_TYPE_PCM_U32LE, CODEC_TYPE_PCM_U32LE_PLANAR, CodecType,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const VERSION: &[u8] = b"musicata-audio-packet-digest-v1";
const MAX_PACKET_BYTES: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

/// Returns a versioned exact identity digest for one complete playable audio stream.
///
/// Container tags are deliberately excluded. Codec parameters, frame timing, packet boundaries,
/// and every encoded packet byte are included. Formats without a complete declared frame count
/// are unverifiable and return an error rather than a partial digest.
pub fn digest_source(source: Box<dyn MediaSource>, extension: &str) -> Result<String, String> {
    let deadline = Instant::now() + TIMEOUT;
    let (source, read_failed) = crate::analysis_source::TrackedSource::new(source);
    let stream = MediaSourceStream::new(
        Box::new(crate::analysis_source::DeadlineSource::new(
            Box::new(source),
            deadline,
        )),
        Default::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("audio identity probe: {error}"))?;
    let mut format = probed.format;
    let playable: Vec<_> = format
        .tracks()
        .iter()
        .filter(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .collect();
    if playable.len() != 1 {
        return Err(format!(
            "audio identity requires exactly one playable stream, found {}",
            playable.len()
        ));
    }
    let track = playable[0];
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let declared = codec_params.n_frames.filter(|n| *n > 0).ok_or_else(|| {
        "audio identity cannot prove complete input: no declared frame count".to_string()
    })?;
    let mut hash = Sha256::new();
    framed(&mut hash, VERSION);
    // This is Symphonia's complete decoder contract: codec, extra data, rate, channels/layout,
    // bit depth, timebase, delay/padding, integrity and verification parameters.
    framed(&mut hash, format!("{codec_params:?}").as_bytes());
    let mut packets = 0u64;
    let mut first = None;
    let mut next = None;
    loop {
        if Instant::now() >= deadline {
            return Err("audio identity exceeded the 120s read budget".into());
        }
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(format!("audio identity packet read: {error}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        if packet.buf().is_empty() || packet.buf().len() > MAX_PACKET_BYTES {
            return Err("audio identity encountered an invalid or oversized packet".into());
        }
        if let Some(bytes_per_sample) = pcm_bytes_per_sample(codec_params.codec) {
            let channels = codec_params
                .channels
                .ok_or_else(|| "audio identity PCM stream has no channel count".to_string())?
                .count() as u64;
            let expected = packet
                .block_dur()
                .checked_mul(channels)
                .and_then(|samples| samples.checked_mul(bytes_per_sample))
                .ok_or_else(|| "audio identity PCM packet size overflow".to_string())?;
            if packet.buf().len() as u64 != expected {
                return Err("audio identity encountered a truncated PCM packet".into());
            }
        }
        if let Some(expected) = next {
            if packet.ts() != expected {
                return Err("audio identity encountered non-contiguous packet timing".into());
            }
        } else {
            first = Some(packet.ts());
        }
        next = Some(
            packet
                .ts()
                .checked_add(packet.block_dur())
                .ok_or_else(|| "audio identity packet timing overflow".to_string())?,
        );
        framed(&mut hash, &packet.ts().to_le_bytes());
        framed(&mut hash, &packet.dur().to_le_bytes());
        framed(&mut hash, &packet.trim_start().to_le_bytes());
        framed(&mut hash, &packet.trim_end().to_le_bytes());
        framed(&mut hash, packet.buf());
        packets += 1;
    }
    if read_failed.load(Ordering::Relaxed) {
        return Err("audio identity source read failed".into());
    }
    if Instant::now() >= deadline {
        return Err("audio identity exceeded the 120s read budget".into());
    }
    let first = first.ok_or_else(|| "audio identity found no audio packets".to_string())?;
    let observed = next
        .expect("packet establishes next timestamp")
        .checked_sub(first)
        .ok_or_else(|| "audio identity packet timing underflow".to_string())?;
    if observed != declared {
        return Err(format!(
            "audio identity incomplete input: declared {declared} frames but read {observed}"
        ));
    }
    framed(&mut hash, &packets.to_le_bytes());
    framed(&mut hash, &observed.to_le_bytes());
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn pcm_bytes_per_sample(codec: CodecType) -> Option<u64> {
    match codec {
        CODEC_TYPE_PCM_S8
        | CODEC_TYPE_PCM_S8_PLANAR
        | CODEC_TYPE_PCM_U8
        | CODEC_TYPE_PCM_U8_PLANAR
        | CODEC_TYPE_PCM_ALAW
        | CODEC_TYPE_PCM_MULAW => Some(1),
        CODEC_TYPE_PCM_S16LE
        | CODEC_TYPE_PCM_S16LE_PLANAR
        | CODEC_TYPE_PCM_S16BE
        | CODEC_TYPE_PCM_S16BE_PLANAR
        | CODEC_TYPE_PCM_U16LE
        | CODEC_TYPE_PCM_U16LE_PLANAR
        | CODEC_TYPE_PCM_U16BE
        | CODEC_TYPE_PCM_U16BE_PLANAR => Some(2),
        CODEC_TYPE_PCM_S24LE
        | CODEC_TYPE_PCM_S24LE_PLANAR
        | CODEC_TYPE_PCM_S24BE
        | CODEC_TYPE_PCM_S24BE_PLANAR
        | CODEC_TYPE_PCM_U24LE
        | CODEC_TYPE_PCM_U24LE_PLANAR
        | CODEC_TYPE_PCM_U24BE
        | CODEC_TYPE_PCM_U24BE_PLANAR => Some(3),
        CODEC_TYPE_PCM_S32LE
        | CODEC_TYPE_PCM_S32LE_PLANAR
        | CODEC_TYPE_PCM_S32BE
        | CODEC_TYPE_PCM_S32BE_PLANAR
        | CODEC_TYPE_PCM_U32LE
        | CODEC_TYPE_PCM_U32LE_PLANAR
        | CODEC_TYPE_PCM_U32BE
        | CODEC_TYPE_PCM_U32BE_PLANAR
        | CODEC_TYPE_PCM_F32LE
        | CODEC_TYPE_PCM_F32LE_PLANAR
        | CODEC_TYPE_PCM_F32BE
        | CODEC_TYPE_PCM_F32BE_PLANAR => Some(4),
        CODEC_TYPE_PCM_F64LE
        | CODEC_TYPE_PCM_F64LE_PLANAR
        | CODEC_TYPE_PCM_F64BE
        | CODEC_TYPE_PCM_F64BE_PLANAR => Some(8),
        _ => None,
    }
}

fn framed(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::digest_source;
    use std::io::{self, Cursor, Read, Seek, SeekFrom};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use symphonia::core::io::MediaSource;

    fn chunk(id: &[u8; 4], body: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 != 0 {
            out.push(0);
        }
    }

    fn wav(samples: &[u8], tags: Option<&[u8]>) -> Vec<u8> {
        let mut chunks = Vec::new();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&8u16.to_le_bytes());
        chunk(b"fmt ", &fmt, &mut chunks);
        if let Some(tags) = tags {
            let mut list = b"INFO".to_vec();
            chunk(b"INAM", tags, &mut list);
            chunk(b"LIST", &list, &mut chunks);
        }
        chunk(b"data", samples, &mut chunks);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(4 + chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&chunks);
        out
    }

    fn wav_pcm16(samples: &[i16]) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let mut chunks = Vec::new();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&16_000u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        chunk(b"fmt ", &fmt, &mut chunks);
        chunk(b"data", &pcm, &mut chunks);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(4 + chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&chunks);
        out
    }

    fn id3_title(title: &str, audio: &[u8]) -> Vec<u8> {
        let mut frame = b"TIT2".to_vec();
        let mut body = vec![3]; // UTF-8 encoding marker.
        body.extend_from_slice(title.as_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&body);
        let size = frame.len() as u32;
        let mut out = b"ID3\x03\0\0".to_vec();
        out.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        out.extend_from_slice(&frame);
        out.extend_from_slice(audio);
        out
    }

    fn flac_comment(title: &str) -> Vec<u8> {
        let entry = format!("TITLE={title}");
        let mut out = (b"musicata".len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(b"musicata");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
        out
    }

    fn retag_flac(audio: &[u8], title: &str) -> Vec<u8> {
        assert_eq!(&audio[..4], b"fLaC");
        let mut out = b"fLaC".to_vec();
        let mut offset = 4;
        loop {
            let header = audio[offset];
            let length = ((audio[offset + 1] as usize) << 16)
                | ((audio[offset + 2] as usize) << 8)
                | audio[offset + 3] as usize;
            let body = &audio[offset + 4..offset + 4 + length];
            let replacement = if header & 0x7f == 4 {
                flac_comment(title)
            } else {
                body.to_vec()
            };
            out.push(header);
            out.extend_from_slice(&(replacement.len() as u32).to_be_bytes()[1..]);
            out.extend_from_slice(&replacement);
            offset += 4 + length;
            if header & 0x80 != 0 {
                break;
            }
        }
        out.extend_from_slice(&audio[offset..]);
        out
    }

    #[test]
    fn ignores_wav_tag_chunks_but_not_audio_packets() {
        let first = wav(&[128, 129, 130, 131], Some(b"INAMfirst"));
        let retagged = wav(&[128, 129, 130, 131], Some(b"INAMsecond title"));
        let changed = wav(&[128, 129, 130, 132], Some(b"INAMfirst"));

        let first_digest = digest_source(Box::new(Cursor::new(first)), "wav").unwrap();
        assert_eq!(
            first_digest,
            digest_source(Box::new(Cursor::new(retagged)), "wav").unwrap()
        );
        assert_ne!(
            first_digest,
            digest_source(Box::new(Cursor::new(changed)), "wav").unwrap()
        );
    }

    #[test]
    fn rejects_truncated_audio_instead_of_digesting_a_prefix() {
        let mut audio = wav(&[128; 128], None);
        audio.truncate(audio.len() - 20);
        assert!(digest_source(Box::new(Cursor::new(audio)), "wav").is_err());
    }

    #[test]
    fn rejects_a_pcm16_packet_missing_its_last_byte() {
        let mut audio = wav_pcm16(&[0, 1, -1, 2]);
        audio.pop();
        assert!(digest_source(Box::new(Cursor::new(audio)), "wav").is_err());
    }

    #[test]
    fn ignores_real_mp3_id3_tag_rewrites() {
        let audio =
            include_bytes!("../../../testdata-fixture/Loose Files/an untagged recording.mp3");
        let first = id3_title("first", audio);
        let retagged = id3_title("a different title", audio);
        assert_eq!(
            digest_source(Box::new(Cursor::new(first)), "mp3").unwrap(),
            digest_source(Box::new(Cursor::new(retagged)), "mp3").unwrap(),
        );
    }

    #[test]
    fn ignores_real_flac_vorbis_comment_rewrites() {
        let audio = include_bytes!(
            "../../../testdata-fixture/Sigrún Ólafsdóttir/Þrír Vetur/01 Þrír Vetur.flac"
        );
        assert_eq!(
            digest_source(Box::new(Cursor::new(retag_flac(audio, "first"))), "flac").unwrap(),
            digest_source(
                Box::new(Cursor::new(retag_flac(audio, "another title"))),
                "flac"
            )
            .unwrap(),
        );
    }

    struct FailingSource {
        bytes: Cursor<Vec<u8>>,
        fail_at: u64,
    }

    impl Read for FailingSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.bytes.position() >= self.fail_at {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "offline"));
            }
            let allowed = (self.fail_at - self.bytes.position()) as usize;
            let length = buf.len().min(allowed);
            self.bytes.read(&mut buf[..length])
        }
    }
    impl Seek for FailingSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(pos)
        }
    }
    impl MediaSource for FailingSource {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.bytes.get_ref().len() as u64)
        }
    }

    #[test]
    fn rejects_source_read_failures() {
        let audio = wav(&[128; 32 * 1024], None);
        let source = FailingSource {
            fail_at: 4_096,
            bytes: Cursor::new(audio),
        };
        assert!(digest_source(Box::new(source), "wav").is_err());
    }

    struct CountedSource {
        bytes: Cursor<Vec<u8>>,
        maximum_request: Arc<AtomicUsize>,
    }
    impl Read for CountedSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.maximum_request.fetch_max(buf.len(), Ordering::Relaxed);
            self.bytes.read(buf)
        }
    }
    impl Seek for CountedSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(pos)
        }
    }
    impl MediaSource for CountedSource {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.bytes.get_ref().len() as u64)
        }
    }

    #[test]
    fn streams_a_large_payload_in_bounded_reads() {
        let maximum_request = Arc::new(AtomicUsize::new(0));
        let source = CountedSource {
            bytes: Cursor::new(wav(&vec![128; 24 * 1024 * 1024], None)),
            maximum_request: maximum_request.clone(),
        };
        assert!(digest_source(Box::new(source), "wav").is_ok());
        assert!(maximum_request.load(Ordering::Relaxed) <= 32 * 1024);
    }
}
