// SPDX-License-Identifier: AGPL-3.0-or-later
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use futures::StreamExt;
use musicata_core::{
    BuiltTrack, DiscoveredAudioFile, FsEntry, Library, PriorIndex, ReadSeek, ScanIssue,
    ScanProgress, SourceFs, assemble_library, build_track, list_dir_audio,
};
use musicata_storage::SourceRecord;

use crate::scan_concurrency::AdaptiveLimiter;
use smb::{
    Client, ClientConfig, ConnectionConfig, FileAccessMask, FileDirectoryInformation, NotifyFilter,
    Resource, UncPath,
    resource::{Directory, File, FileCreateArgs, GetLen, ReadAt},
};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

/// Block size for the read-ahead cache in [`SmbReadSeek`]. lofty parses tags with
/// many small seeks/reads; serving them from a cached block turns a burst of tiny
/// SMB round-trips into one.
const READ_BLOCK_BYTES: usize = 256 * 1024;

/// Chunk size for HTTP range streaming. Each chunk is one SMB read sent to the client
/// before the next is fetched, so the first audio bytes arrive after one read rather
/// than buffering the whole range (Jellyfin's model: a chunked copy, playback starting
/// on the first chunk). Unlike Jellyfin's 80 KiB (local files, no per-read latency),
/// SMB pays a round-trip per read, so too-small chunks throttle the fill rate and
/// delay `canplay`. Measured click→audible for a FLAC over SMB by chunk size:
/// 128 KiB→515 ms, 256 KiB→335 ms, 512 KiB→226 ms, 1 MiB→176 ms. 1 MiB is the knee —
/// it streams the whole file (no follow-up range request) with bounded memory
/// (`channel` depth × this size).
const STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Adaptive scan concurrency bounds: start moderate, back off to a safe floor on
/// stress, probe up to a generous ceiling on a healthy NAS.
const INITIAL_CONCURRENCY: usize = 10;
const MIN_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY: usize = 50;

/// Number of independent SMB connections to open per share for scanning. One
/// connection caps read throughput; a few scale it on a capable NAS. Overridable
/// via `MUSICATA_SMB_CONNECTIONS` (mainly for tuning/benchmarking).
const DEFAULT_SMB_CONNECTIONS: usize = 4;

fn smb_pool_size() -> usize {
    std::env::var("MUSICATA_SMB_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SMB_CONNECTIONS)
        .clamp(1, 16)
}

/// Current unix time in seconds (mirrors core's private helper; used as the scan's
/// observation timestamp).
fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Walk the share's directory tree in parallel — list directories concurrently,
/// level by level, bounded by the shared adaptive limiter — instead of one folder
/// at a time. Returns the audio files plus any walk issues, reporting discovery
/// progress after each level.
async fn parallel_walk<F>(
    fs: Arc<SmbFs>,
    root: PathBuf,
    limiter: Arc<AdaptiveLimiter>,
    progress: &mut F,
) -> (Vec<DiscoveredAudioFile>, Vec<ScanIssue>)
where
    F: FnMut(ScanProgress) + Send,
{
    let mut files = Vec::new();
    let mut issues = Vec::new();
    let mut frontier = vec![root];
    while !frontier.is_empty() {
        let level: Vec<_> = futures::stream::iter(frontier)
            .map(|dir| {
                let fs = fs.clone();
                let limiter = limiter.clone();
                async move {
                    let permit = limiter.acquire().await;
                    let started = Instant::now();
                    let listing =
                        tokio::task::spawn_blocking(move || list_dir_audio(&*fs, &dir)).await;
                    limiter.record(started.elapsed());
                    drop(permit);
                    listing
                }
            })
            .buffer_unordered(MAX_CONCURRENCY)
            .collect()
            .await;

        let mut next = Vec::new();
        for joined in level {
            match joined {
                Ok(mut listing) => {
                    files.append(&mut listing.files);
                    issues.append(&mut listing.issues);
                    next.append(&mut listing.subdirs);
                }
                Err(error) => issues.push(ScanIssue {
                    path: String::new(),
                    message: format!("walk task panicked: {error}"),
                }),
            }
        }
        frontier = next;
        progress(ScanProgress::Discovering { found: files.len() });
    }
    (files, issues)
}

/// Build the live progress detail string: throughput, ETA, and parallelism.
fn scan_detail(done: usize, total: usize, start: Instant, concurrency: usize) -> String {
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let rate = done as f64 / elapsed;
    let remaining = total.saturating_sub(done);
    let eta_secs = if rate > 0.0 {
        (remaining as f64 / rate) as u64
    } else {
        0
    };
    let eta = if eta_secs >= 60 {
        format!("{}m{:02}s", eta_secs / 60, eta_secs % 60)
    } else {
        format!("{eta_secs}s")
    };
    format!("{rate:.0}/s · ETA {eta} · {concurrency} parallel")
}

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
    let base = base_path.replace('\\', "/");
    let base = base.trim_matches('/');
    // Drop `.`/`..`/empty segments from the untrusted relative path so an item id can't
    // traverse above the configured base.
    let relative = relative
        .replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .collect::<Vec<_>>()
        .join("/");
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

/// How long to wait for an SMB connection before giving up. Without this an
/// unreachable host blocks the request (and the rescan lock) indefinitely.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long to wait for a single SMB read/open before giving up. A healthy read is tens of ms
/// on a 1 GbE LAN; this only fires on a wedged connection, so a stuck cover/stream can't hang
/// the request — and the browser `<img>`/`<audio>` waiting on it — indefinitely. On a timeout
/// the slot is invalidated so the next request reconnects rather than reusing the dead client.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long an outstanding SMB2 change-notify waits before re-arming. CHANGE_NOTIFY is
/// one-shot (one batch per request), so the watch loop re-issues after each batch; this bounds
/// how long a single outstanding request lives, re-arming on idle so a silently-dropped
/// connection surfaces (the re-arm read fails) rather than hanging forever.
const WATCH_REARM: std::time::Duration = std::time::Duration::from_secs(300);

/// Client config that permits guest/anonymous shares — those sessions can't sign,
/// so signing must be allowed to skip for them or the connect is rejected.
fn client_config() -> ClientConfig {
    ClientConfig {
        connection: ConnectionConfig {
            allow_unsigned_guest_access: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn new_client() -> Arc<Client> {
    Arc::new(Client::new(client_config()))
}

/// Connect a client to the share, failing fast on an unreachable host or bad
/// credentials instead of hanging. An empty username connects as `guest` (an empty
/// identity is rejected outright by NTLM), which is how open shares expect access.
async fn connect_share(client: &Client, config: &SmbConfig) -> anyhow::Result<()> {
    let target = config.share_unc()?;
    let username = if config.username.is_empty() {
        "guest"
    } else {
        &config.username
    };
    let connect = client.share_connect(&target, username, config.password.clone());
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("SMB connect to {}: {error}", config.host)),
        Err(_) => Err(anyhow!(
            "SMB connect to {} timed out after {}s",
            config.host,
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

fn smb_io_error(error: smb::Error) -> io::Error {
    io::Error::other(error.to_string())
}

/// Whether an SMB error reflects transport/connection stress (worth backing off the
/// scan concurrency) rather than a normal protocol response. A `ReceivedErrorMessage`
/// means the server answered (e.g. "file not found" when probing for a `.lrc`
/// sidecar) — the connection is healthy, so it must NOT drive backoff.
fn is_transport_stress(error: &smb::Error) -> bool {
    matches!(
        error,
        smb::Error::TransportError(_) | smb::Error::OperationTimeout(..) | smb::Error::IoError(_)
    )
}

fn filetime_to_unix(time: smb::binrw_util::file_time::FileTime) -> Option<i64> {
    SystemTime::from(time)
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

/// A small set of independent SMB connections for serving. SMB2 multiplexes concurrent
/// operations over one connection via credits, but a single connection still caps throughput
/// and serializes the tail under a burst — a cover grid on first load measured ~2.6 s wall /
/// p95 2.5 s over one connection. A few connections, used round-robin, spread that load so
/// covers don't starve the stream. Each slot is established lazily and reconnected on error.
struct SmbPool {
    config: SmbConfig,
    slots: Vec<Mutex<Option<Arc<Client>>>>,
    next: AtomicUsize,
}

impl SmbPool {
    fn new(config: SmbConfig) -> Self {
        let slots = (0..smb_pool_size()).map(|_| Mutex::new(None)).collect();
        Self {
            config,
            slots,
            next: AtomicUsize::new(0),
        }
    }

    /// Get a client from the next slot (round-robin), establishing it on first use. Returns the
    /// slot index too, so the caller can invalidate just that slot on a failed operation.
    async fn get(&self) -> anyhow::Result<(usize, Arc<Client>)> {
        let slot = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut guard = self.slots[slot].lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok((slot, client.clone()));
        }
        let client = new_client();
        connect_share(&client, &self.config).await?;
        *guard = Some(client.clone());
        Ok((slot, client))
    }

    /// Drop a slot's client so its next use reconnects — called when an op on it fails or times
    /// out (e.g. the NAS rebooted), otherwise later requests would reuse the dead connection.
    async fn invalidate(&self, slot: usize) {
        if let Some(cell) = self.slots.get(slot) {
            *cell.lock().await = None;
        }
    }

    /// Establish every slot up front so the first request doesn't pay the connect latency
    /// (measured ~436 ms cold vs ~35 ms warm for a stream's first byte). Errors are logged,
    /// never fatal — a slot reconnects lazily on next use.
    async fn warm(&self) {
        for slot in 0..self.slots.len() {
            if let Err(error) = self.get().await {
                tracing::warn!(%error, slot, "smb: warm connect failed");
            }
        }
    }
}

/// An SMB music source. Holds connection parameters and a small pool of lazily-established,
/// reused clients for streaming and cover fetches.
pub struct SmbProvider {
    provider_id: String,
    config: SmbConfig,
    pool: Arc<SmbPool>,
}

impl SmbProvider {
    pub fn from_record(record: &SourceRecord) -> anyhow::Result<Self> {
        let config = SmbConfig::from_record(record)?;
        Ok(Self {
            provider_id: record.id.clone(),
            pool: Arc::new(SmbPool::new(config.clone())),
            config,
        })
    }

    /// Establish the serving connection pool up front (off the request hot path) so the first
    /// play/cover doesn't pay the cold-connect cost.
    pub async fn warm(&self) {
        self.pool.warm().await;
    }

    pub fn provider_id(&self) -> &String {
        &self.provider_id
    }

    /// Scan the share into a [`Library`] with **adaptive concurrency**: many files'
    /// tags are read in parallel over one connection (SMB2 multiplexes via credits),
    /// the parallelism climbing while healthy and backing off on I/O errors / latency.
    /// Connecting + the directory walk + tag parsing are blocking (`block_on`), so the
    /// per-file work runs on blocking threads; only new/changed files are read
    /// (incremental). Reports live throughput / ETA / parallelism.
    pub async fn scan_with_progress<F>(
        &self,
        prior: Option<Arc<Library>>,
        mut progress: F,
    ) -> anyhow::Result<Library>
    where
        F: FnMut(ScanProgress) + Send + 'static,
    {
        let config = self.config.clone();
        let provider_id = self.provider_id.clone();
        let handle = Handle::current();

        // Connect on a blocking thread (block_on), then walk the tree in parallel.
        progress(ScanProgress::Discovering { found: 0 });
        let fs = tokio::task::spawn_blocking({
            let config = config.clone();
            let handle = handle.clone();
            move || SmbFs::connect(config, handle)
        })
        .await??;

        let root = fs.root().to_path_buf();
        let observed_at = current_unix_seconds();
        let index = Arc::new(PriorIndex::from_library(prior.as_deref(), &provider_id));
        let fs = Arc::new(fs);

        // Each phase gets its own adaptive limiter: directory listings and tag reads
        // have very different latencies, so sharing one would let the fast walk poison
        // the read phase's latency baseline (making every read look "slow" and pinning
        // concurrency). Both still share the io-error counter.
        let walk_limiter = AdaptiveLimiter::new(
            INITIAL_CONCURRENCY,
            MIN_CONCURRENCY,
            MAX_CONCURRENCY,
            fs.io_errors(),
        );

        // Parallel directory walk: list directories concurrently (one round-trip
        // each), level by level, instead of one-at-a-time.
        let (mut files, walk_errors) =
            parallel_walk(fs.clone(), root.clone(), walk_limiter, &mut progress).await;
        files.sort_by(|left, right| left.path.cmp(&right.path));

        let total = files.len();
        progress(ScanProgress::Discovered { files: total });

        let limiter = AdaptiveLimiter::new(
            INITIAL_CONCURRENCY,
            MIN_CONCURRENCY,
            MAX_CONCURRENCY,
            fs.io_errors(),
        );
        let scan_start = Instant::now();
        let mut last_report = Instant::now();
        let mut built: Vec<BuiltTrack> = Vec::with_capacity(total);

        let task_root = root.clone();
        let task_limiter = limiter.clone();
        let mut stream = futures::stream::iter(files)
            .map(move |file| {
                let fs = fs.clone();
                let index = index.clone();
                let limiter = task_limiter.clone();
                let provider_id = provider_id.clone();
                let root = task_root.clone();
                async move {
                    let permit = limiter.acquire().await;
                    let started = Instant::now();
                    let result = tokio::task::spawn_blocking(move || {
                        build_track(&*fs, &root, &provider_id, &file, &index, observed_at)
                    })
                    .await;
                    limiter.record(started.elapsed());
                    drop(permit);
                    result
                }
            })
            .buffer_unordered(MAX_CONCURRENCY);

        while let Some(joined) = stream.next().await {
            match joined {
                Ok(item) => built.push(item),
                Err(error) => tracing::warn!(%error, "scan task panicked"),
            }
            if last_report.elapsed() >= Duration::from_millis(300) || built.len() == total {
                progress(ScanProgress::Processing {
                    done: built.len(),
                    total,
                    detail: Some(scan_detail(
                        built.len(),
                        total,
                        scan_start,
                        limiter.current(),
                    )),
                });
                last_report = Instant::now();
            }
        }

        let provider_id = self.provider_id.clone();
        let library = tokio::task::spawn_blocking(move || {
            assemble_library(&provider_id, &root, built, walk_errors)
        })
        .await?;
        Ok(library)
    }

    /// Read `[start, end]` (inclusive) of an item for streaming. Only the requested
    /// window is fetched — a multi-GB file is never buffered whole.
    /// Stream the byte range `start..=end` from the share in [`STREAM_CHUNK_BYTES`]
    /// chunks. Each chunk is read from SMB and handed to the response body before the
    /// next is fetched, so the first bytes reach the client after a single read instead
    /// of buffering the whole range (which, for an open-ended `bytes=0-` from a browser,
    /// meant pulling the entire file over the wire before playback could start). The
    /// reader runs in its own task feeding a small bounded channel, so if the client
    /// stops draining (paused/seeked/closed) the channel fills, the task parks, and no
    /// further SMB reads happen — natural backpressure, no whole-file buffering.
    pub async fn read_range_stream(
        &self,
        item_id: &str,
        start: u64,
        end: u64,
    ) -> anyhow::Result<impl futures::Stream<Item = io::Result<Vec<u8>>> + Send + 'static> {
        let (slot, client) = self.pool.get().await?;
        let unc = self.config.item_unc(item_id)?;
        let resource =
            match tokio::time::timeout(READ_TIMEOUT, client.create_file(&unc, &open_read_args()))
                .await
            {
                Ok(Ok(resource)) => resource,
                Ok(Err(error)) => {
                    // The connection may be dead — drop this slot so the next call reconnects.
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("open {item_id}: {error}"));
                }
                Err(_) => {
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("open {item_id}: timed out"));
                }
            };
        let Resource::File(file) = resource else {
            return Err(anyhow!("{item_id} is not a file"));
        };

        // A small bounded channel gives backpressure: at most a few chunks buffered.
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Vec<u8>>>(4);
        let label = item_id.to_string();
        let pool = self.pool.clone();
        tokio::spawn(async move {
            // Keep the client alive for the lifetime of the read.
            let _client = client;
            let mut offset = start;
            while offset <= end {
                let want = ((end - offset + 1).min(STREAM_CHUNK_BYTES as u64)) as usize;
                let mut buffer = vec![0u8; want];
                match tokio::time::timeout(READ_TIMEOUT, file.read_at(&mut buffer, offset)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(read)) => {
                        buffer.truncate(read);
                        if tx.send(Ok(buffer)).await.is_err() {
                            break; // client went away
                        }
                        offset += read as u64;
                    }
                    Ok(Err(error)) => {
                        let _ = tx
                            .send(Err(io::Error::other(format!("smb read {label}: {error}"))))
                            .await;
                        pool.invalidate(slot).await;
                        break;
                    }
                    Err(_) => {
                        let _ = tx
                            .send(Err(io::Error::other(format!(
                                "smb read {label}: timed out"
                            ))))
                            .await;
                        pool.invalidate(slot).await;
                        break;
                    }
                }
            }
        });
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
    }

    /// Read a whole (small) file by its share-relative path — used for cover art,
    /// which is fetched in full rather than by range. Reads in chunks until EOF so a
    /// modest buffer is held regardless of the image size.
    pub async fn read_file(&self, item_id: &str) -> anyhow::Result<Vec<u8>> {
        let (slot, client) = self.pool.get().await?;
        let unc = self.config.item_unc(item_id)?;
        let resource =
            match tokio::time::timeout(READ_TIMEOUT, client.create_file(&unc, &open_read_args()))
                .await
            {
                Ok(Ok(resource)) => resource,
                Ok(Err(error)) => {
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("open {item_id}: {error}"));
                }
                Err(_) => {
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("open {item_id}: timed out"));
                }
            };
        let Resource::File(file) = resource else {
            return Err(anyhow!("{item_id} is not a file"));
        };

        const CHUNK: usize = 256 * 1024;
        // Cap total read so a mis-pointed huge file can't exhaust memory.
        const MAX: usize = 32 * 1024 * 1024;
        let mut out: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let read = match tokio::time::timeout(
                READ_TIMEOUT,
                file.read_at(&mut chunk, out.len() as u64),
            )
            .await
            {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => {
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("read {item_id}: {error}"));
                }
                Err(_) => {
                    self.pool.invalidate(slot).await;
                    return Err(anyhow!("read {item_id}: timed out"));
                }
            };
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read]);
            if out.len() >= MAX {
                anyhow::bail!("{item_id} exceeds the {MAX}-byte artwork limit");
            }
        }
        Ok(out)
    }

    /// Watch the share for changes over SMB2 CHANGE_NOTIFY (recursive), sending `()` on `tx` for
    /// each batch of notifications — the network equivalent of a local filesystem watcher, so new
    /// or edited files trigger a targeted rescan instead of waiting for the periodic safety-net
    /// pass. Uses its own dedicated connection (a watch is long-lived and must not tie up the
    /// serving pool). Returns `Err` on a connection failure so the caller can reconnect (and do a
    /// catch-up scan for anything missed during the gap); an idle window just re-arms the watch.
    pub async fn watch_changes(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> anyhow::Result<()> {
        let client = new_client();
        connect_share(&client, &self.config).await?;
        let unc = self.config.item_unc("")?;
        let Resource::Directory(dir) = client
            .create_file(&unc, &open_read_args())
            .await
            .map_err(|error| anyhow!("open share root for watch: {error}"))?
        else {
            anyhow::bail!("share root is not a directory");
        };
        loop {
            // Notify on the changes that affect the library: files/dirs added, removed, or
            // renamed, and content edits (size / last-write).
            let filter = NotifyFilter::new()
                .with_file_name(true)
                .with_dir_name(true)
                .with_size(true)
                .with_last_write(true);
            match dir.watch_timeout(filter, true, WATCH_REARM).await {
                Ok(_changes) => {
                    if tx.send(()).is_err() {
                        return Ok(()); // receiver gone — stop watching
                    }
                }
                // No changes within the window — re-arm (also surfaces a dead connection).
                Err(smb::Error::OperationTimeout(..)) => {}
                Err(error) => return Err(anyhow!("smb change-notify: {error}")),
            }
        }
    }

    /// Verify the source is reachable: connect and enumerate the root directory.
    /// Used to reject a misconfigured source at add time, before any scan.
    pub async fn validate(&self) -> anyhow::Result<()> {
        let (_slot, client) = self.pool.get().await?;
        let unc = self.config.item_unc("")?;
        let resource = client
            .create_file(&unc, &open_read_args())
            .await
            .map_err(|error| anyhow!("open share: {error}"))?;
        let directory = match resource {
            Resource::Directory(directory) => Arc::new(directory),
            _ => return Err(anyhow!("path is not a directory")),
        };
        let mut stream = Directory::query::<FileDirectoryInformation>(&directory, "*")
            .await
            .map_err(|error| anyhow!("list directory: {error}"))?;
        // Pull the first entry so an enumerate error (vs. an empty share) surfaces.
        if let Some(entry) = stream.next().await {
            entry.map_err(|error| anyhow!("read directory: {error}"))?;
        }
        Ok(())
    }
}

/// A [`SourceFs`] backed by a synchronous view of an SMB share. Every method
/// drives the async client via `Handle::block_on`; this is only valid on a
/// blocking thread (the scanner runs under `spawn_blocking`).
struct SmbFs {
    /// A pool of independent connections to the share; scan reads are spread across
    /// them round-robin. A single connection caps throughput (~one in-flight stream
    /// of completions); several connections scale it on a capable NAS.
    clients: Vec<Arc<Client>>,
    next: std::sync::atomic::AtomicUsize,
    config: SmbConfig,
    handle: Handle,
    /// Logical root for the scanner; SMB items hang off "/".
    root: PathBuf,
    /// Cumulative count of SMB I/O errors (not parse failures) — the adaptive
    /// concurrency limiter watches this to back off when the share is struggling.
    io_errors: Arc<AtomicU64>,
}

impl SmbFs {
    fn connect(config: SmbConfig, handle: Handle) -> anyhow::Result<Self> {
        let pool_size = smb_pool_size();
        let mut clients = Vec::with_capacity(pool_size);
        for index in 0..pool_size {
            let client = new_client();
            match handle.block_on(connect_share(&client, &config)) {
                Ok(()) => clients.push(client),
                // The first connection must succeed; spares failing is non-fatal.
                Err(error) if index == 0 => return Err(error),
                Err(error) => {
                    tracing::warn!(%error, "extra SMB connection failed; continuing with fewer");
                    break;
                }
            }
        }
        Ok(Self {
            clients,
            next: std::sync::atomic::AtomicUsize::new(0),
            config,
            handle,
            root: PathBuf::from("/"),
            io_errors: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Pick the next pooled connection, round-robin.
    fn client(&self) -> &Arc<Client> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        &self.clients[index]
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

    /// Shared cumulative I/O-error counter (for the adaptive concurrency limiter).
    fn io_errors(&self) -> Arc<AtomicU64> {
        self.io_errors.clone()
    }

    /// Map an SMB error to an `io::Error`, counting it toward the adaptive limiter's
    /// backoff signal only when it's transport stress (not a benign protocol response
    /// like a missing sidecar probe, and not a tag-parse failure).
    fn io_err(&self, error: smb::Error) -> io::Error {
        if is_transport_stress(&error) {
            self.io_errors.fetch_add(1, Ordering::Relaxed);
        }
        smb_io_error(error)
    }

    fn open_file(&self, logical: &Path) -> io::Result<File> {
        let unc = self.unc(logical).map_err(io::Error::other)?;
        let resource = self
            .handle
            .block_on(self.client().create_file(&unc, &open_read_args()))
            .map_err(|error| self.io_err(error))?;
        match resource {
            Resource::File(file) => Ok(file),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a file",
            )),
        }
    }
}

impl SourceFs for SmbFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
        let unc = self.unc(dir).map_err(io::Error::other)?;
        self.handle.block_on(async {
            let resource = self
                .client()
                .create_file(&unc, &open_read_args())
                .await
                .map_err(|error| self.io_err(error))?;
            let Resource::Directory(directory) = resource else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path is not a directory",
                ));
            };
            let directory: Arc<Directory> = Arc::new(directory);
            let mut stream = Directory::query::<FileDirectoryInformation>(&directory, "*")
                .await
                .map_err(|error| self.io_err(error))?;
            let mut entries = Vec::new();
            while let Some(item) = stream.next().await {
                let info = item.map_err(|error| self.io_err(error))?;
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
        let len = self
            .handle
            .block_on(file.get_len())
            .map_err(|error| self.io_err(error))?;
        let reader = SmbFileReader {
            file,
            handle: self.handle.clone(),
            io_errors: self.io_errors.clone(),
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
                .client()
                .create_file(&unc, &open_read_args())
                .await
                .map_err(|error| self.io_err(error))?;
            let (is_dir, size) = match resource.as_file() {
                Some(file) => (false, file.get_len().await.ok()),
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
    io_errors: Arc<AtomicU64>,
}

impl BlockingReadAt for SmbFileReader {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.handle
            .block_on(self.file.read_at(buf, offset))
            .map_err(|error| {
                if is_transport_stress(&error) {
                    self.io_errors.fetch_add(1, Ordering::Relaxed);
                }
                smb_io_error(error)
            })
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

    #[test]
    fn join_smb_path_neutralizes_traversal() {
        // ISS-22: a `..` in the (untrusted) relative item id must not escape the base.
        assert_eq!(join_smb_path("Music", "../secret"), "Music/secret");
        assert_eq!(join_smb_path("Music", "a/../../b"), "Music/a/b");
        assert_eq!(join_smb_path("Music", "./x.flac"), "Music/x.flac");
        // Backslashes in the base are normalized too (consistent with smb_provider_id).
        assert_eq!(join_smb_path("Music\\Sub", "x.flac"), "Music/Sub/x.flac");
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
