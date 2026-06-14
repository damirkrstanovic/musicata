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

/// The fixed stream name snapserver advertises for Musicata's own decoded library audio.
pub const STREAM_NAME: &str = "Musicata";
/// Stream id for the AirPlay input (snapserver spawns shairport-sync and exposes it as a stream).
pub const AIRPLAY_STREAM_NAME: &str = "AirPlay";
/// Stream id for the Spotify Connect input (snapserver spawns librespot).
pub const SPOTIFY_STREAM_NAME: &str = "Spotify";

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
}

impl SnapcastManager {
    /// Ensure the FIFO exists and — when managing — snapserver is running against it.
    pub async fn start(settings: SnapcastSettings) -> Result<Self> {
        ensure_fifo(&settings.fifo_path).await?;
        let mut child = None;
        if settings.manage_server {
            child = Some(spawn_server(&settings).await?);
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
            settings,
            child: Mutex::new(child),
        })
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
            && let Some(child) = guard.as_mut() {
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
        "[stream]\n\
         source = pipe://{fifo}?name={name}&sampleformat={rate}:16:{channels}&codec=flac&mode=read\n",
        fifo = settings.fifo_path.display(),
        name = STREAM_NAME,
        rate = settings.sample_rate,
        channels = CHANNELS,
    );
    // Optional inputs *into* Musicata. AirPlay/librespot run at 44.1 kHz; the per-stream
    // sampleformat says so (only one stream plays per group at a time, so mixed rates are fine).
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

    fn room(name: &str, pw: &str) -> SnapRoom {
        SnapRoom {
            name: name.to_string(),
            password: pw.to_string(),
        }
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
