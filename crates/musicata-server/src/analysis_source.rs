// SPDX-License-Identifier: AGPL-3.0-or-later
//! Streaming analysis must distinguish a transport failure from a bad audio file.
//! Some demuxers treat a failed read as EOF; keep that evidence outside the decoder.
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;
use symphonia::core::io::MediaSource;

pub struct TrackedSource {
    inner: Box<dyn MediaSource>,
    failed: Arc<AtomicBool>,
}

impl TrackedSource {
    pub fn new(inner: Box<dyn MediaSource>) -> (Self, Arc<AtomicBool>) {
        let failed = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner,
                failed: failed.clone(),
            },
            failed,
        )
    }
}

impl Read for TrackedSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(out);
        if result.is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}
impl Seek for TrackedSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let result = self.inner.seek(pos);
        if result.is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}
impl MediaSource for TrackedSource {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// A `MediaSource` that stops serving bytes once a wall-clock deadline passes. Format probing
/// (which can scan a whole malformed file before the decode loop even starts) and every
/// `next_packet` read flow through this, so the deadline bounds *all* the decode work — not just
/// the loop. After the deadline, reads return `TimedOut`, so `symphonia` aborts with an error.
pub(crate) struct DeadlineSource {
    inner: Box<dyn MediaSource>,
    deadline: Instant,
}

impl DeadlineSource {
    pub(crate) fn new(inner: Box<dyn MediaSource>, deadline: Instant) -> Self {
        Self { inner, deadline }
    }
}

impl Read for DeadlineSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "audio analysis budget exceeded",
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
        self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Offline;
    impl Read for Offline {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "offline"))
        }
    }
    impl Seek for Offline {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }
    impl MediaSource for Offline {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(100)
        }
    }

    #[test]
    fn decoder_failure_keeps_source_error_retryable() {
        let (source, failed) = TrackedSource::new(Box::new(Offline));
        assert!(crate::fingerprint::compute_fingerprint_source(Box::new(source), "wav").is_err());
        assert!(failed.load(Ordering::Relaxed));
        let (source, failed) = TrackedSource::new(Box::new(std::io::Cursor::new(vec![0; 64])));
        assert!(crate::fingerprint::compute_fingerprint_source(Box::new(source), "wav").is_err());
        assert!(
            !failed.load(Ordering::Relaxed),
            "invalid audio is not a network failure"
        );
    }
}
