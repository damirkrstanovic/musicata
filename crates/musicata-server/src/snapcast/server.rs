// SPDX-License-Identifier: AGPL-3.0-or-later
//! snapserver lifecycle + the single "Musicata" pipe stream.
//!
//! Snapcast broadcasts a continuous PCM stream to N synchronized clients (rooms). We run
//! one managed `snapserver` fed by one FIFO ("Musicata" stream); every assigned snapclient
//! plays it sample-accurately in sync. That is the multi-room MVP — one stream, many synced
//! rooms. (Multiple independent streams — different music per room — is a future extension;
//! snapserver defines pipe streams at startup, so it would mean regenerating the config and
//! restarting.) Musicata has never managed a subprocess before; this mirrors the planned
//! CamillaDSP management and is gated behind the `snapcast` cargo feature.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
// std (not tokio) process: snapserver is a fire-and-forget long-lived child we never await
// output from, and an unwaited `tokio::process::Child` makes the runtime's SIGCHLD reaper
// busy-spin. We kill (+ reap) it explicitly on shutdown/drop instead.
use std::process::{Child, Command};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::decode::CHANNELS;

/// Preserve Snapserver's conservative default until the user's network is tested.
pub const DEFAULT_BUFFER_MS: u32 = 1000;

pub fn validate_buffer_ms(value: u32) -> Result<()> {
    if !(100..=5000).contains(&value) {
        return Err(anyhow!("Playback buffer must be between 100 and 5000 ms"));
    }
    Ok(())
}

/// The fixed stream name Snapserver advertises for Musicata's decoded library audio.
pub const STREAM_NAME: &str = "Musicata";
/// Stream id for the AirPlay input (snapserver spawns shairport-sync and exposes it as a stream).
pub const AIRPLAY_STREAM_NAME: &str = "AirPlay";
/// Stream id for the Spotify Connect input (snapserver spawns librespot).
pub const SPOTIFY_STREAM_NAME: &str = "Spotify";
pub const SETTING_MPD_INPUT: &str = "snapcast.mpd_enabled";
pub const MPD_FIFO_PATH: &str = "/var/lib/musicata-mpd/snapcast.fifo";

/// Resolve a binary name (or path) to an existing file: an absolute/relative path is checked
/// directly, a bare name is searched on `$PATH`. Used both to build the stream `source = ` URI
/// and to tell the UI whether the binary is installed.
pub fn resolve_binary(name: &str) -> Option<PathBuf> {
    let candidate = std::path::Path::new(name);
    if name.contains('/') {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let full = dir.join(name);
        full.is_file().then_some(full)
    })
}

/// Whether a binary (name or path) is present — for the "shairport-sync not found" UI hint.
pub fn binary_present(name: &str) -> bool {
    resolve_binary(name).is_some()
}

/// Percent-encode a value for a snapserver `source = ` query parameter (device names may carry
/// spaces or reserved characters).
fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A provisioned room: a name (also the snapclient's auth username + display id) and its
/// generated password. Persisted as JSON in the `snapcast.rooms` setting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapRoom {
    pub name: String,
    pub password: String,
}

/// Settings for the Snapcast subsystem, persisted in the DB and edited in `/admin`
/// (configuration lives in the product, not in flags).
#[derive(Clone, Debug)]
pub struct SnapcastSettings {
    /// Master switch. When off, no snapserver is started and no Snapcast player exists.
    pub enabled: bool,
    /// Let Musicata start/stop `snapserver` itself. When false, connect to one the user
    /// already runs (we only create/own the FIFO + talk JSON-RPC).
    pub manage_server: bool,
    /// The `snapserver` binary (looked up on `PATH` by default).
    pub server_binary: String,
    /// FIFO Musicata writes decoded PCM into and snapserver reads.
    pub fifo_path: PathBuf,
    /// Stream sample rate (snapserver runs at one fixed rate; we resample to it).
    pub sample_rate: u32,
    pub buffer_ms: u32,
    pub mpd_enabled: bool,
    pub mpd_fifo_path: PathBuf,
    /// snapserver JSON-RPC control endpoint (TCP, newline-delimited).
    pub control_host: String,
    pub control_port: u16,
    /// snapserver HTTP/Snapweb port (the bundled browser client + WS control).
    pub http_port: u16,
    /// Require a per-room password (writes an `[authorization]` block). NOTE: snapserver 0.35
    /// **ignores** this (`auth.enabled` is hardcoded off upstream — `// TODO: auth`), so it is
    /// not enforced today; the config + passwords are forward-compatible for when upstream
    /// ships auth. See `docs/snapcast.md`.
    pub auth_enabled: bool,
    /// The address rooms dial (`tcp://<name>:<pw>@<host>:1704`) — shown in the install command.
    pub server_host: String,
    /// Provisioned rooms (name + password). Written as `authorization.user` entries.
    pub rooms: Vec<SnapRoom>,
    /// Accept AirPlay *into* Musicata: snapserver spawns `shairport-sync` and exposes it as the
    /// `AirPlay` stream, which rooms can be switched to. Requires shairport-sync installed.
    pub airplay_enabled: bool,
    /// The `shairport-sync` binary (name on `PATH` or a path).
    pub airplay_binary: String,
    /// The name the AirPlay receiver advertises (what shows in the phone's AirPlay picker).
    pub airplay_device_name: String,
    /// Accept Spotify Connect *into* Musicata: snapserver spawns `librespot` and exposes it as
    /// the `Spotify` stream. Requires librespot installed (and a Spotify account on the phone).
    pub spotify_enabled: bool,
    /// The `librespot` binary (name on `PATH` or a path).
    pub spotify_binary: String,
    /// The name the Spotify Connect device advertises.
    pub spotify_device_name: String,
    /// librespot stream bitrate (96/160/320).
    pub spotify_bitrate: u32,
}

impl Default for SnapcastSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            manage_server: true,
            server_binary: "snapserver".to_string(),
            fifo_path: std::env::temp_dir().join("musicata-snapcast/musicata.fifo"),
            sample_rate: 48_000,
            buffer_ms: DEFAULT_BUFFER_MS,
            mpd_enabled: false,
            mpd_fifo_path: MPD_FIFO_PATH.into(),
            control_host: "127.0.0.1".to_string(),
            control_port: 1705,
            http_port: 1780,
            auth_enabled: false,
            server_host: "localhost".to_string(),
            rooms: Vec::new(),
            airplay_enabled: false,
            airplay_binary: "shairport-sync".to_string(),
            airplay_device_name: "Musicata".to_string(),
            spotify_enabled: false,
            spotify_binary: "librespot".to_string(),
            spotify_device_name: "Musicata".to_string(),
            spotify_bitrate: 320,
        }
    }
}

/// Owns the FIFO and (optionally) the snapserver child process. Dropping it kills a
/// managed child.
pub struct SnapcastManager {
    settings: SnapcastSettings,
    child: Mutex<Option<Child>>,
    buffer_ms: std::sync::atomic::AtomicU32,
    mpd_enabled: std::sync::atomic::AtomicBool,
    apply_lock: tokio::sync::Mutex<()>,
}

impl SnapcastManager {
    /// Ensure the FIFO exists and — when managing — snapserver is running against it.
    pub async fn start(settings: SnapcastSettings) -> Result<Self> {
        validate_buffer_ms(settings.buffer_ms)?;
        if settings.mpd_enabled {
            validate_mpd_fifo(&settings.mpd_fifo_path).await?;
        }
        ensure_fifo(&settings.fifo_path).await?;
        let mut child = None;
        if settings.manage_server {
            child = Some(start_checked(&settings).await?);
            tracing::info!(
                stream = STREAM_NAME,
                fifo = %settings.fifo_path.display(),
                "snapcast: started managed snapserver"
            );
        } else {
            tracing::info!(
                fifo = %settings.fifo_path.display(),
                "snapcast: using external snapserver"
            );
        }
        Ok(Self {
            buffer_ms: std::sync::atomic::AtomicU32::new(settings.buffer_ms),
            mpd_enabled: std::sync::atomic::AtomicBool::new(settings.mpd_enabled),
            apply_lock: tokio::sync::Mutex::new(()),
            settings,
            child: Mutex::new(child),
        })
    }

    pub fn managed(&self) -> bool {
        self.settings.manage_server
    }

    pub fn is_running(&self) -> bool {
        !self.managed()
            || self
                .child
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|child| child.try_wait().is_ok_and(|exit| exit.is_none()))
    }

    pub fn buffer_ms(&self) -> Option<u32> {
        (self.managed() && self.is_running())
            .then(|| self.buffer_ms.load(std::sync::atomic::Ordering::Acquire))
    }

    pub fn mpd_enabled(&self) -> bool {
        self.mpd_enabled.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Apply the input on packaged Snapserver versions, including those without AddStream.
    pub async fn apply_mpd_and_save(
        self: std::sync::Arc<Self>,
        database: musicata_storage::Database,
        enabled: bool,
    ) -> Result<()> {
        tokio::spawn(async move {
            let _apply = self.apply_lock.lock().await;
            let previous = self.mpd_enabled();
            self.apply_mpd(enabled).await?;
            if let Err(error) = database.set_bool_setting(SETTING_MPD_INPUT, enabled).await {
                self.apply_mpd(previous).await.map_err(|restore| {
                    anyhow!("Could not save MPD input ({error}) or restore it ({restore})")
                })?;
                return Err(error.into());
            }
            Ok(())
        })
        .await?
    }

    async fn apply_mpd(&self, enabled: bool) -> Result<()> {
        if !self.managed() {
            return Err(anyhow!(
                "Configure the MPD input on the external Snapserver"
            ));
        }
        if enabled == self.mpd_enabled() {
            return Ok(());
        }
        if enabled {
            validate_mpd_fifo(&self.settings.mpd_fifo_path).await?;
        }
        let mut previous = self.settings.clone();
        previous.buffer_ms = self.buffer_ms.load(std::sync::atomic::Ordering::Acquire);
        previous.mpd_enabled = self.mpd_enabled();
        let mut candidate = previous.clone();
        candidate.mpd_enabled = enabled;
        self.replace_server(&candidate, &previous).await?;
        self.mpd_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Complete process replacement and persistence even if the HTTP caller disconnects.
    pub async fn apply_buffer_and_save(
        self: std::sync::Arc<Self>,
        database: musicata_storage::Database,
        value: u32,
    ) -> Result<()> {
        tokio::spawn(async move {
            let _apply = self.apply_lock.lock().await;
            let previous = self.buffer_ms().ok_or_else(|| {
                anyhow!("Snapserver is not running; restart Musicata before applying the buffer")
            })?;
            self.apply_buffer(value).await?;
            if let Err(error) = database
                .set_setting("snapcast.buffer_ms", &value.to_string())
                .await
            {
                self.apply_buffer(previous).await.map_err(|restore| {
                    anyhow!("Could not save buffer ({error}) or restore it ({restore})")
                })?;
                return Err(error.into());
            }
            Ok(())
        })
        .await?
    }

    /// Caller holds apply_lock through persistence as well as process replacement.
    async fn apply_buffer(&self, value: u32) -> Result<()> {
        validate_buffer_ms(value)?;
        if !self.managed() {
            return Err(anyhow!(
                "Configure the playback buffer on the external Snapserver"
            ));
        }
        let previous = self.buffer_ms.load(std::sync::atomic::Ordering::Acquire);
        if previous == value {
            return Ok(());
        }
        let mut candidate = self.settings.clone();
        candidate.buffer_ms = value;
        candidate.mpd_enabled = self.mpd_enabled();
        let mut rollback = candidate.clone();
        rollback.buffer_ms = previous;
        self.replace_server(&candidate, &rollback).await?;
        self.buffer_ms
            .store(value, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    async fn replace_server(
        &self,
        candidate: &SnapcastSettings,
        rollback: &SnapcastSettings,
    ) -> Result<()> {
        if self.mpd_enabled() {
            let streams = super::control::SnapControl::new(self.control_addr())
                .streams()
                .await?;
            if !streams
                .iter()
                .any(|stream| stream.id == "MPD" && stream.status == "idle")
            {
                return Err(anyhow!(
                    "Pause MPD and wait for its input to become idle before changing the playback buffer or MPD input"
                ));
            }
        }
        let old_child = self.child.lock().unwrap().take();
        if let Some(child) = old_child {
            stop_server(child).await;
        }
        match start_checked(candidate).await {
            Ok(child) => {
                *self.child.lock().unwrap() = Some(child);
                Ok(())
            }
            Err(error) => match start_checked(rollback).await {
                Ok(child) => {
                    *self.child.lock().unwrap() = Some(child);
                    Err(anyhow!(
                        "Snapserver change failed; previous configuration restored: {error}"
                    ))
                }
                Err(restore) => Err(anyhow!(
                    "Snapserver change failed: {error}; restoring previous server also failed: {restore}"
                )),
            },
        }
    }

    pub fn fifo_path(&self) -> PathBuf {
        self.settings.fifo_path.clone()
    }

    pub fn sample_rate(&self) -> u32 {
        self.settings.sample_rate
    }

    pub fn control_addr(&self) -> String {
        format!(
            "{}:{}",
            self.settings.control_host, self.settings.control_port
        )
    }
}

impl Drop for SnapcastManager {
    fn drop(&mut self) {
        // Best-effort: if `shutdown` wasn't called, kill the child so we don't leak a
        // snapserver process.
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Create the FIFO if it isn't already one. A stale non-FIFO at the path is replaced.
async fn ensure_fifo(path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create FIFO dir {}", parent.display()))?;
    }
    if is_fifo(path).await {
        return Ok(());
    }
    // Replace a stale regular file/dir at the path, then make the FIFO via `mkfifo`
    // (universally present on the target platforms; avoids a libc dependency).
    let _ = tokio::fs::remove_file(path).await;
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .with_context(|| "spawn mkfifo (is coreutils installed?)")?;
    if !status.success() {
        return Err(anyhow!("mkfifo {} failed: {status}", path.display()));
    }
    Ok(())
}

/// Whether `path` exists and is a FIFO.
async fn is_fifo(path: &PathBuf) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta.file_type().is_fifo(),
        Err(_) => false,
    }
}

async fn validate_mpd_fifo(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = tokio::fs::symlink_metadata(path).await.with_context(
        || "MPD FIFO is unavailable; provision MPD through Snapcast with the installer",
    )?;
    if !metadata.file_type().is_fifo() {
        return Err(anyhow!(
            "MPD input requires a real FIFO, not a regular file or symlink"
        ));
    }
    Ok(())
}

fn mpd_stream_uri(settings: &SnapcastSettings) -> String {
    format!(
        "pipe://{}?name=MPD&sampleformat=48000:16:2&codec=flac&mode=read",
        settings.mpd_fifo_path.display()
    )
}

/// The forward-compatible `[authorization]` block: a `Streaming`-only role and one user per
/// room. **snapserver 0.35 ignores this** (auth is stubbed upstream); it activates unchanged
/// if/when upstream enables auth.
fn authorization_block(rooms: &[SnapRoom]) -> String {
    let mut block = String::from(
        "[authorization]\n\
         enabled = true\n\
         role = stream:Streaming\n",
    );
    for room in rooms {
        block.push_str(&format!("user = {}:{}:stream\n", room.name, room.password));
    }
    block
}

/// The `snapclient` command to run on a room device: connects with the room's password and
/// identifies itself by name. (The password is sent today but not enforced by snapserver
/// 0.35 — see `docs/snapcast.md`; it becomes enforcing when upstream ships auth.)
pub fn install_command(host: &str, room: &SnapRoom) -> String {
    format!(
        "snapclient 'tcp://{name}:{pw}@{host}:1704' --hostID '{name}'",
        name = room.name,
        pw = room.password,
    )
}

/// Render the snapserver config: the Musicata library pipe stream, any enabled AirPlay/Spotify
/// input streams (snapserver spawns shairport-sync/librespot itself — skipped with a warning
/// when the binary is missing so a missing dep degrades gracefully), the control/HTTP ports, and
/// the forward-compatible auth block. Pure (no IO) so it's unit-testable.
fn render_config(settings: &SnapcastSettings) -> String {
    // Musicata's own decoded library audio.
    let mut config = format!(
        "[server]\n\
         datadir = {state_dir}\n\
         [stream]\n\
         buffer = {buffer}\n\
         source = pipe://{fifo}?name={name}&sampleformat={rate}:16:{channels}&codec=flac&mode=read\n",
        // The service account may have no home directory (especially in Docker).
        // ensure_fifo creates this writable directory before spawning Snapserver.
        state_dir = settings
            .fifo_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .display(),
        fifo = settings.fifo_path.display(),
        name = STREAM_NAME,
        rate = settings.sample_rate,
        channels = CHANNELS,
        buffer = settings.buffer_ms,
    );
    // Optional inputs *into* Musicata. AirPlay/librespot run at 44.1 kHz; the per-stream
    // sampleformat says so (only one stream plays per group at a time, so mixed rates are fine).
    if settings.mpd_enabled {
        config.push_str(&format!("source = {}\n", mpd_stream_uri(settings)));
    }
    if settings.airplay_enabled {
        match resolve_binary(&settings.airplay_binary) {
            Some(bin) => config.push_str(&format!(
                "[stream]\n\
                 source = airplay://{bin}?name={name}&devicename={device}&port=7000&sampleformat=44100:16:2\n",
                bin = bin.display(),
                name = AIRPLAY_STREAM_NAME,
                device = query_encode(&settings.airplay_device_name),
            )),
            None => tracing::warn!(
                binary = %settings.airplay_binary,
                "snapcast: AirPlay input enabled but shairport-sync not found; skipping the stream"
            ),
        }
    }
    if settings.spotify_enabled {
        match resolve_binary(&settings.spotify_binary) {
            Some(bin) => config.push_str(&format!(
                "[stream]\n\
                 source = librespot://{bin}?name={name}&devicename={device}&bitrate={bitrate}&sampleformat=44100:16:2\n",
                bin = bin.display(),
                name = SPOTIFY_STREAM_NAME,
                device = query_encode(&settings.spotify_device_name),
                bitrate = settings.spotify_bitrate,
            )),
            None => tracing::warn!(
                binary = %settings.spotify_binary,
                "snapcast: Spotify input enabled but librespot not found; skipping the stream"
            ),
        }
    }
    config.push_str(&format!(
        "[tcp]\n\
         enabled = true\n\
         port = {tcp}\n\
         [http]\n\
         enabled = true\n\
         port = {http}\n",
        tcp = settings.control_port,
        http = settings.http_port,
    ));
    if settings.auth_enabled {
        config.push_str(&authorization_block(&settings.rooms));
    }
    config
}

/// Own every intermediate process across await points, including task cancellation.
struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn stop_server(child: Child) {
    let mut guard = ChildGuard(Some(child));
    let child = guard.0.as_mut().unwrap();
    // SIGTERM lets Snapserver persist client volume/group/stream state in its datadir.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    for _ in 0..80 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
}

async fn start_checked(settings: &SnapcastSettings) -> Result<Child> {
    let mut guard = ChildGuard(Some(spawn_server(settings).await?));
    let control = super::control::SnapControl::new(format!(
        "{}:{}",
        settings.control_host, settings.control_port
    ));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = guard.0.as_mut().unwrap().try_wait()? {
            return Err(anyhow!("Snapserver exited during apply: {status}"));
        }
        match tokio::time::timeout_at(deadline, control.streams()).await {
            Ok(Ok(_)) => return Ok(guard.0.take().unwrap()),
            result if tokio::time::Instant::now() >= deadline => {
                return Err(anyhow!("Snapserver did not become ready: {result:?}"));
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

/// Write the snapserver config for our streams and spawn the process.
async fn spawn_server(settings: &SnapcastSettings) -> Result<Child> {
    let config_path = settings
        .fifo_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("snapserver.conf");
    let config = render_config(settings);
    tokio::fs::write(&config_path, config)
        .await
        .with_context(|| format!("write snapserver config {}", config_path.display()))?;

    let mut command = Command::new(&settings.server_binary);
    command.arg("-c").arg(&config_path);
    // Tie snapserver's lifetime to ours: if Musicata dies *without* running Drop (a crash or
    // SIGKILL), the kernel sends the child SIGKILL too. Without this a crash-restart would
    // leave the old snapserver reading the FIFO while the new one starts a second reader —
    // two readers split the byte stream and both get a corrupt, gappy feed.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().with_context(|| {
        format!(
            "spawn {} (is snapserver installed?)",
            settings.server_binary
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn test_dir() -> TestDir {
        let path = std::env::temp_dir().join(format!(
            "musicata-buffer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        TestDir(path)
    }

    fn room(name: &str, pw: &str) -> SnapRoom {
        SnapRoom {
            name: name.to_string(),
            password: pw.to_string(),
        }
    }

    #[test]
    fn mpd_input_is_opt_in_and_uses_its_own_fixed_format_fifo() {
        let mut settings = SnapcastSettings::default();
        assert!(!render_config(&settings).contains("name=MPD"));
        settings.mpd_enabled = true;
        assert!(render_config(&settings).contains(
            "pipe:///var/lib/musicata-mpd/snapcast.fifo?name=MPD&sampleformat=48000:16:2&codec=flac&mode=read"
        ));
        assert!(render_config(&settings).contains("name=Musicata"));
    }

    #[tokio::test]
    async fn mpd_input_rejects_missing_regular_and_symlink_paths_without_replacing_them() {
        let root = test_dir();
        let fifo = root.path().join("mpd.fifo");
        assert!(validate_mpd_fifo(&fifo).await.is_err());
        std::fs::write(&fifo, "preserve").unwrap();
        assert!(validate_mpd_fifo(&fifo).await.is_err());
        assert_eq!(std::fs::read_to_string(&fifo).unwrap(), "preserve");
        std::fs::remove_file(&fifo).unwrap();
        ensure_fifo(&fifo).await.unwrap();
        validate_mpd_fifo(&fifo).await.unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&fifo, &alias).unwrap();
        assert!(validate_mpd_fifo(&alias).await.is_err());
    }

    #[test]
    fn playback_buffer_is_explicit_and_validated() {
        let mut settings = SnapcastSettings::default();
        assert!(render_config(&settings).contains("buffer = 1000\n"));
        settings.buffer_ms = 250;
        assert!(render_config(&settings).contains("buffer = 250\n"));
        assert!(validate_buffer_ms(250).is_ok());
        assert!(validate_buffer_ms(0).is_err());
        assert!(validate_buffer_ms(5001).is_err());
    }

    #[tokio::test]
    async fn buffer_apply_restarts_and_rolls_back_failed_process() {
        use std::os::unix::fs::PermissionsExt;
        let root = test_dir();
        let binary = root.path().join("fake-snapserver");
        std::fs::write(
            &binary,
            r#"#!/usr/bin/python3
import sys, socket, json, re, time
config = open(sys.argv[2]).read()
if 'buffer = 250\n' in config: sys.exit(1)
time.sleep(0.5) # Healthy slow startup must be retried, including rollback.
port = int(re.search(r'\[tcp\]\nenabled = true\nport = (\d+)', config).group(1))
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', port)); s.listen()
while True:
 c, _ = s.accept()
 with c:
  c.recv(65536)
  c.sendall((json.dumps({'id': 1, 'result': {'server': {'streams': []}}})+'\n').encode())
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let manager = SnapcastManager::start(SnapcastSettings {
            server_binary: binary.to_string_lossy().into_owned(),
            fifo_path: root.path().join("audio.fifo"),
            control_port: port,
            ..Default::default()
        })
        .await
        .unwrap();
        let manager = std::sync::Arc::new(manager);
        let database = musicata_storage::Database::connect(root.path().join("settings.db"))
            .await
            .unwrap();
        let pending = tokio::spawn(manager.clone().apply_buffer_and_save(database.clone(), 500));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !std::fs::read_to_string(root.path().join("snapserver.conf"))
                .unwrap()
                .contains("buffer = 500\n")
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Cancel the caller while the slow candidate has not bound its port yet.
        pending.abort();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while database
                .get_setting("snapcast.buffer_ms")
                .await
                .unwrap()
                .as_deref()
                != Some("500")
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.buffer_ms(), Some(500));
        let error = manager.apply_buffer(250).await.unwrap_err().to_string();
        assert!(error.contains("previous configuration restored"), "{error}");
        assert_eq!(manager.buffer_ms(), Some(500));
        assert!(
            manager
                .child
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        assert!(
            std::fs::read_to_string(root.path().join("snapserver.conf"))
                .unwrap()
                .contains("buffer = 500\n")
        );
    }

    #[tokio::test]
    async fn external_server_buffer_is_not_reported_or_modified() {
        let root = test_dir();
        let manager = SnapcastManager::start(SnapcastSettings {
            manage_server: false,
            fifo_path: root.path().join("audio.fifo"),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(manager.buffer_ms(), None);
        assert!(manager.apply_buffer(250).await.is_err());
        assert!(!root.path().join("snapserver.conf").exists());
    }

    #[test]
    fn authorization_block_lists_one_user_per_room() {
        let block = authorization_block(&[room("kitchen", "aaa"), room("living", "bbb")]);
        assert!(block.contains("[authorization]"));
        assert!(block.contains("role = stream:Streaming"));
        assert!(block.contains("user = kitchen:aaa:stream"));
        assert!(block.contains("user = living:bbb:stream"));
    }

    #[test]
    fn install_command_embeds_name_and_password() {
        let cmd = install_command("musicata.local", &room("kitchen", "secret"));
        assert_eq!(
            cmd,
            "snapclient 'tcp://kitchen:secret@musicata.local:1704' --hostID 'kitchen'"
        );
    }

    #[test]
    fn render_config_has_only_the_library_stream_by_default() {
        let config = render_config(&SnapcastSettings::default());
        assert_eq!(config.matches("[stream]").count(), 1);
        assert!(config.contains(&format!("name={STREAM_NAME}")));
        assert!(!config.contains("airplay://"));
        assert!(!config.contains("librespot://"));
    }

    #[test]
    fn managed_state_uses_writable_fifo_directory() {
        let settings = SnapcastSettings {
            fifo_path: "/data/snapcast/audio.fifo".into(),
            ..SnapcastSettings::default()
        };
        assert!(render_config(&settings).contains("[server]\ndatadir = /data/snapcast\n"));
    }

    #[test]
    fn render_config_adds_input_streams_when_enabled_and_binary_present() {
        // Use a binary that exists on every dev/CI box so resolution succeeds.
        let mut settings = SnapcastSettings {
            airplay_enabled: true,
            airplay_binary: "/bin/sh".to_string(),
            airplay_device_name: "Living Room".to_string(),
            spotify_enabled: true,
            spotify_binary: "/bin/sh".to_string(),
            spotify_bitrate: 320,
            ..SnapcastSettings::default()
        };
        let config = render_config(&settings);
        assert_eq!(config.matches("[stream]").count(), 3);
        assert!(config.contains(&format!("airplay:///bin/sh?name={AIRPLAY_STREAM_NAME}")));
        assert!(config.contains("devicename=Living%20Room")); // spaces are encoded
        assert!(config.contains(&format!("librespot:///bin/sh?name={SPOTIFY_STREAM_NAME}")));
        assert!(config.contains("bitrate=320"));

        // A missing binary is skipped (graceful), leaving only the library + spotify streams.
        settings.airplay_binary = "/nonexistent/shairport-sync".to_string();
        let config = render_config(&settings);
        assert_eq!(config.matches("[stream]").count(), 2);
        assert!(!config.contains("airplay://"));
    }

    #[test]
    fn resolve_binary_finds_absolute_paths_and_misses() {
        assert!(resolve_binary("/bin/sh").is_some());
        assert!(resolve_binary("/nope/definitely-not-here").is_none());
        assert!(binary_present("/bin/sh"));
    }
}
