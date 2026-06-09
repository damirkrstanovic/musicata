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

/// The fixed stream name snapserver advertises and we point groups at.
pub const STREAM_NAME: &str = "Musicata";

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

    /// Stop a managed snapserver (no-op for an external one). Kills and reaps it.
    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().expect("snapcast child lock").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SnapcastManager {
    fn drop(&mut self) {
        // Best-effort: if `shutdown` wasn't called, kill the child so we don't leak a
        // snapserver process.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
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

/// Write a minimal snapserver config for our pipe stream and spawn the process.
async fn spawn_server(settings: &SnapcastSettings) -> Result<Child> {
    let config_path = settings
        .fifo_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("snapserver.conf");
    let mut config = format!(
        "[stream]\n\
         source = pipe://{fifo}?name={name}&sampleformat={rate}:16:{channels}&codec=flac&mode=read\n\
         [tcp]\n\
         enabled = true\n\
         port = {tcp}\n\
         [http]\n\
         enabled = true\n\
         port = {http}\n",
        fifo = settings.fifo_path.display(),
        name = STREAM_NAME,
        rate = settings.sample_rate,
        channels = CHANNELS,
        tcp = settings.control_port,
        http = settings.http_port,
    );
    if settings.auth_enabled {
        config.push_str(&authorization_block(&settings.rooms));
    }
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
}
