//! SMB/CIFS music source — reads a network share directly over the wire (pure
//! Rust, no kernel mount, no libsmbclient) via the `smb` crate.
//!
//! Two execution models, because the `smb` crate is async-only:
//! - **Scanning** runs the shared sync [`scan_source`] over an [`SmbFs`]. Each
//!   `SourceFs` call drives the async client with `Handle::block_on`, which is
//!   safe *only* on a blocking thread — so scanning always runs inside
//!   `spawn_blocking` (block_on panics on a tokio worker thread).
//! - **Streaming** is already async (the HTTP handler), so it awaits the client
//!   directly and never blocks; the connection is cached and reused across range
//!   requests.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use futures::StreamExt;
use musicata_core::{FsEntry, Library, ReadSeek, SourceFs, scan_source};
use musicata_storage::SourceRecord;
use smb::{
    Client, ClientConfig, FileAccessMask, FileDirectoryInformation, UncPath,
    resource::{Directory, File, FileCreateArgs, GetLen, ReadAt},
};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

/// Block size for the read-ahead cache in [`SmbReadSeek`]. lofty parses tags with
/// many small seeks/reads; serving them from a cached block turns a burst of tiny
/// SMB round-trips into one.
const READ_BLOCK_BYTES: usize = 256 * 1024;

/// Connection parameters for an SMB share.
#[derive(Clone, Debug)]
pub struct SmbConfig {
    pub host: String,
    pub share: String,
    /// Subfolder within the share to treat as the source root (may be empty).
    pub base_path: String,
    pub username: String,
    pub password: String,
}

impl SmbConfig {
    fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let host = record
            .host
            .clone()
            .filter(|value| !value.is_empty())
            .context("SMB source is missing a host")?;
        let share = record
            .share
            .clone()
            .filter(|value| !value.is_empty())
            .context("SMB source is missing a share")?;
        Ok(Self {
            host,
            share,
            base_path: record.base_path.clone().unwrap_or_default(),
            // SMB needs a domain-qualified username; the bare name works for most
            // setups and an empty username means a guest/anonymous mount.
            username: record.username.clone().unwrap_or_default(),
            password: record.password.clone().unwrap_or_default(),
        })
    }

    /// The `\\host\share` target used to authenticate.
    fn share_unc(&self) -> anyhow::Result<UncPath> {
        UncPath::new(&self.host)
            .and_then(|unc| unc.with_share(&self.share))
            .map_err(|error| anyhow!("invalid SMB host/share: {error}"))
    }

    /// The full UNC path to an item, given its source-relative path.
    fn item_unc(&self, relative: &str) -> anyhow::Result<UncPath> {
        let combined = join_smb_path(&self.base_path, relative);
        let unc = self.share_unc()?;
        Ok(if combined.is_empty() {
            unc.with_no_path()
        } else {
            unc.with_path(&combined)
        })
    }
}

/// Join a source base path with a source-relative path into one share-relative
/// path (forward slashes; the `smb` crate normalizes separators).
fn join_smb_path(base_path: &str, relative: &str) -> String {
    let base = base_path.trim_matches(['/', '\\']);
    let relative = relative.trim_matches(['/', '\\']).replace('\\', "/");
    match (base.is_empty(), relative.is_empty()) {
        (true, true) => String::new(),
        (true, false) => relative,
        (false, true) => base.to_string(),
        (false, false) => format!("{base}/{relative}"),
    }
}

fn open_read_args() -> FileCreateArgs {
    FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true))
}

fn smb_io_error(error: smb::Error) -> io::Error {
    io::Error::other(error.to_string())
}

fn filetime_to_unix(time: smb::binrw_util::file_time::FileTime) -> Option<i64> {
    SystemTime::from(time)
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

/// An SMB music source. Holds connection parameters and a lazily-established,
/// reused client for streaming.
pub struct SmbProvider {
    provider_id: String,
    config: SmbConfig,
    client: Mutex<Option<Arc<Client>>>,
}

impl SmbProvider {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        Ok(Self {
            provider_id: record.id.clone(),
            config: SmbConfig::from_record(record)?,
            client: Mutex::new(None),
        })
    }

    pub fn provider_id(&self) -> &String {
        &self.provider_id
    }

    /// Scan the share into a [`Library`]. Connection + tag parsing are blocking,
    /// so this runs on a blocking thread where `block_on` is legal.
    pub async fn scan(&self) -> anyhow::Result<Library> {
        let config = self.config.clone();
        let provider_id = self.provider_id.clone();
        let handle = Handle::current();
        tokio::task::spawn_blocking(move || {
            let fs = SmbFs::connect(config, handle)?;
            scan_source(&fs, &provider_id).map_err(|error| anyhow!(error.to_string()))
        })
        .await?
    }

    /// Read `[start, end]` (inclusive) of an item for streaming. Only the requested
    /// window is fetched — a multi-GB file is never buffered whole.
    pub async fn read_range(&self, item_id: &str, start: u64, end: u64) -> anyhow::Result<Vec<u8>> {
        if end < start {
            return Ok(Vec::new());
        }
        let client = self.client().await?;
        let unc = self.config.item_unc(item_id)?;
        let file = client
            .create_file(&unc, &open_read_args())
            .await
            .map_err(|error| anyhow!("open {item_id}: {error}"))?
            .unwrap_file();

        let want = (end - start + 1) as usize;
        let mut buffer = vec![0u8; want];
        let mut filled = 0usize;
        while filled < want {
            let read = file
                .read_at(&mut buffer[filled..], start + filled as u64)
                .await
                .map_err(|error| anyhow!("read {item_id}: {error}"))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        Ok(buffer)
    }

    /// Get (or establish) the shared client connected to the share.
    async fn client(&self) -> anyhow::Result<Arc<Client>> {
        let mut guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }
        let client = Arc::new(Client::new(ClientConfig::default()));
        let target = self.config.share_unc()?;
        client
            .share_connect(&target, &self.config.username, self.config.password.clone())
            .await
            .map_err(|error| anyhow!("SMB connect to {}: {error}", self.config.host))?;
        *guard = Some(client.clone());
        Ok(client)
    }
}

/// A [`SourceFs`] backed by a synchronous view of an SMB share. Every method
/// drives the async client via `Handle::block_on`; this is only valid on a
/// blocking thread (the scanner runs under `spawn_blocking`).
struct SmbFs {
    client: Arc<Client>,
    config: SmbConfig,
    handle: Handle,
    /// Logical root for the scanner; SMB items hang off "/".
    root: PathBuf,
}

impl SmbFs {
    fn connect(config: SmbConfig, handle: Handle) -> anyhow::Result<Self> {
        let client = Arc::new(Client::new(ClientConfig::default()));
        let target = config.share_unc()?;
        handle
            .block_on(client.share_connect(&target, &config.username, config.password.clone()))
            .map_err(|error| anyhow!("SMB connect to {}: {error}", config.host))?;
        Ok(Self {
            client,
            config,
            handle,
            root: PathBuf::from("/"),
        })
    }

    /// Translate a logical scanner path (under `root`) to its share UNC path.
    fn unc(&self, logical: &Path) -> anyhow::Result<UncPath> {
        let relative = logical
            .strip_prefix(&self.root)
            .unwrap_or(logical)
            .to_string_lossy()
            .to_string();
        self.config.item_unc(&relative)
    }

    fn open_file(&self, logical: &Path) -> io::Result<File> {
        let unc = self.unc(logical).map_err(io::Error::other)?;
        let resource = self
            .handle
            .block_on(self.client.create_file(&unc, &open_read_args()))
            .map_err(smb_io_error)?;
        Ok(resource.unwrap_file())
    }
}

impl SourceFs for SmbFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
        let unc = self.unc(dir).map_err(io::Error::other)?;
        self.handle.block_on(async {
            let resource = self
                .client
                .create_file(&unc, &open_read_args())
                .await
                .map_err(smb_io_error)?;
            let directory: Arc<Directory> = Arc::new(resource.unwrap_dir());
            let mut stream = Directory::query::<FileDirectoryInformation>(&directory, "*")
                .await
                .map_err(smb_io_error)?;
            let mut entries = Vec::new();
            while let Some(item) = stream.next().await {
                let info = item.map_err(smb_io_error)?;
                let name = info.file_name.to_string();
                if name == "." || name == ".." {
                    continue;
                }
                let is_dir = info.file_attributes.directory();
                entries.push(FsEntry {
                    path: dir.join(&name),
                    is_dir,
                    is_file: !is_dir,
                    size: Some(info.end_of_file),
                    modified_at_unix_seconds: filetime_to_unix(info.last_write_time),
                });
            }
            Ok(entries)
        })
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + '_>> {
        let file = self.open_file(path)?;
        let len = self.handle.block_on(file.get_len()).map_err(smb_io_error)?;
        let reader = SmbFileReader {
            file,
            handle: self.handle.clone(),
        };
        Ok(Box::new(CachingReader::new(reader, len)))
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let mut reader = self.open(path)?;
        let mut contents = String::new();
        reader.read_to_string(&mut contents)?;
        Ok(contents)
    }

    fn stat(&self, path: &Path) -> io::Result<FsEntry> {
        let unc = self.unc(path).map_err(io::Error::other)?;
        self.handle.block_on(async {
            let resource = self
                .client
                .create_file(&unc, &open_read_args())
                .await
                .map_err(smb_io_error)?;
            let (is_dir, size) = match resource.as_file() {
                Some(file) => (false, file.get_len().await.map_err(smb_io_error).ok()),
                None => (true, None),
            };
            Ok(FsEntry {
                path: path.to_path_buf(),
                is_dir,
                is_file: !is_dir,
                size,
                modified_at_unix_seconds: None,
            })
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

/// A blocking positioned reader. Abstracts the one network call so the caching
/// `Read + Seek` adapter can be unit-tested against an in-memory fake.
trait BlockingReadAt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// Count of underlying reads issued — used by tests to assert the cache works.
    #[cfg_attr(not(test), allow(dead_code))]
    fn reads(&self) -> usize {
        0
    }
}

/// Drives `smb::File::read_at` synchronously from a blocking thread.
struct SmbFileReader {
    file: File,
    handle: Handle,
}

impl BlockingReadAt for SmbFileReader {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.handle
            .block_on(self.file.read_at(buf, offset))
            .map_err(smb_io_error)
    }
}

/// `Read + Seek` over a [`BlockingReadAt`], with a single read-ahead block cache
/// so lofty's many small seeks don't each cost a round-trip.
struct CachingReader<R: BlockingReadAt> {
    reader: R,
    pos: u64,
    len: u64,
    cache: Vec<u8>,
    cache_start: u64,
}

impl<R: BlockingReadAt> CachingReader<R> {
    fn new(reader: R, len: u64) -> Self {
        Self {
            reader,
            pos: 0,
            len,
            cache: Vec::new(),
            cache_start: 0,
        }
    }

    fn cache_covers(&self, pos: u64) -> bool {
        !self.cache.is_empty()
            && pos >= self.cache_start
            && pos < self.cache_start + self.cache.len() as u64
    }

    fn fill_cache(&mut self) -> io::Result<()> {
        let start = self.pos;
        let want = READ_BLOCK_BYTES.min((self.len - start) as usize);
        let mut block = vec![0u8; want];
        let mut filled = 0usize;
        while filled < want {
            let read = self
                .reader
                .read_at(&mut block[filled..], start + filled as u64)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        block.truncate(filled);
        self.cache = block;
        self.cache_start = start;
        Ok(())
    }
}

impl<R: BlockingReadAt> Read for CachingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        if !self.cache_covers(self.pos) {
            self.fill_cache()?;
        }
        let offset = (self.pos - self.cache_start) as usize;
        if offset >= self.cache.len() {
            return Ok(0);
        }
        let available = &self.cache[offset..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: BlockingReadAt> Seek for CachingReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => self.len as i128 + offset as i128,
            SeekFrom::Current(offset) => self.pos as i128 + offset as i128,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = (target as u64).min(self.len);
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn joins_smb_paths() {
        assert_eq!(join_smb_path("", ""), "");
        assert_eq!(join_smb_path("", "Artist/Song.flac"), "Artist/Song.flac");
        assert_eq!(join_smb_path("Music", ""), "Music");
        assert_eq!(
            join_smb_path("Music", "Artist/x.flac"),
            "Music/Artist/x.flac"
        );
        assert_eq!(join_smb_path("/Music/", "/Artist/"), "Music/Artist");
        assert_eq!(
            join_smb_path("Music", "Artist\\Album\\x.flac"),
            "Music/Artist/Album/x.flac"
        );
    }

    /// In-memory positioned reader that counts how many `read_at` calls it serves.
    struct FakeReader {
        data: Vec<u8>,
        reads: Cell<usize>,
    }

    impl BlockingReadAt for FakeReader {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            let offset = offset as usize;
            if offset >= self.data.len() {
                return Ok(0);
            }
            let available = &self.data[offset..];
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            Ok(n)
        }
        fn reads(&self) -> usize {
            self.reads.get()
        }
    }

    fn caching_reader(data: Vec<u8>) -> CachingReader<FakeReader> {
        let len = data.len() as u64;
        CachingReader::new(
            FakeReader {
                data,
                reads: Cell::new(0),
            },
            len,
        )
    }

    #[test]
    fn reads_whole_file_sequentially() {
        let data: Vec<u8> = (0..1000u32).map(|n| n as u8).collect();
        let mut reader = caching_reader(data.clone());
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn seek_and_read_returns_correct_bytes() {
        let data: Vec<u8> = (0..5000u32).map(|n| n as u8).collect();
        let mut reader = caching_reader(data.clone());

        reader.seek(SeekFrom::Start(100)).unwrap();
        let mut buf = [0u8; 10];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[100..110]);

        // Seek from end, then read.
        reader.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, data[4996..]);

        // Seek before start is rejected; past end clamps to len (yields EOF).
        assert!(reader.seek(SeekFrom::Start(0)).is_ok());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        assert_eq!(reader.seek(SeekFrom::Start(99999)).unwrap(), 5000);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn block_cache_coalesces_small_reads() {
        // A file smaller than one block: many small sequential reads should hit the
        // network exactly once (the single block fill).
        let data: Vec<u8> = (0..1000u32).map(|n| n as u8).collect();
        let mut reader = caching_reader(data);
        let mut byte = [0u8; 1];
        for _ in 0..1000 {
            reader.read_exact(&mut byte).unwrap();
        }
        assert_eq!(reader.reader.reads(), 1, "1000 small reads → 1 SMB read");
    }
}
