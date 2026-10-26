// SPDX-License-Identifier: AGPL-3.0-or-later
use super::http::ExportRequest;
use crate::AppState;
use musicata_storage::diagnostics::DiagnosticSnapshot;
use std::path::{Path, PathBuf};
pub fn path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".diagnostics.zip");
    PathBuf::from(name)
}
pub async fn prepare(state: &AppState, request: ExportRequest) -> anyhow::Result<()> {
    let since = request
        .problem_time_unix_seconds
        .map(|time| time.saturating_sub(3600))
        .unwrap_or(super::now() - 14 * 86400);
    let persistent = if let Some(store) = state.diagnostics.store().await {
        tokio::time::timeout(std::time::Duration::from_secs(2), store.snapshot(since))
            .await
            .ok()
            .and_then(Result::ok)
    } else {
        None
    };
    let history_scope = if persistent.is_some() {
        "persistent_history"
    } else {
        "pending_memory_only"
    };
    let snapshot = persistent.unwrap_or_else(|| state.diagnostics.memory_snapshot());
    let player_context = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.database.list_players(),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let output_context_unavailable = player_context.is_none();
    let players = player_context.unwrap_or_default();
    let kinds: Vec<_> = players
        .iter()
        .take(256)
        .map(|player| serde_json::json!({"id":super::identity(&player.id),"kind":match player.kind.as_str(){"browser"=>"browser","mpd"=>"mpd","snapcast"=>"snapcast",_=>"other"}}))
        .collect();
    let pending = state.diagnostics.memory_snapshot();
    let context = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"history_scope":history_scope,"output_context_unavailable":output_context_unavailable,"outputs":kinds,"omitted_outputs":players.len().saturating_sub(256),"recorder":state.diagnostics.health(),
        "pending_evidence_may_overlap_history":{"incidents":pending.incidents,"transitions":pending.transitions,"performance":pending.performance},
        "description":request.description.map(|text|sanitize_description(&text)),"problem_time_unix_seconds":request.problem_time_unix_seconds,
        "detection_limits":["No analogue or DAC-side glitch measurement","Stream headers do not establish completed playback","Offline reporters have bounded queues"]});
    #[cfg(feature = "snapcast")]
    let context = {
        let mut context = context;
        context["snapcast"] = state.players.snapcast_health.export_summary();
        context
    };
    let destination = path(&state.db_path);
    let mut attachments = vec![(
        "runtime.json",
        serde_json::to_vec(&state.diagnostics.runtime().snapshot(true))?,
    )];
    let profile_status = state.diagnostics.profiles().status();
    let mut omitted_profiles = 0;
    if request.include_profiles {
        for (kind, ready) in [
            (super::profiles::ProfileKind::Cpu, profile_status.cpu_ready),
            (
                super::profiles::ProfileKind::Heap,
                profile_status.heap_ready,
            ),
        ] {
            let profile_path = super::profiles::path(&state.db_path, kind);
            if ready
                && tokio::fs::metadata(&profile_path)
                    .await
                    .is_ok_and(|m| m.len() <= super::profiles::ARTIFACT_LIMIT as u64)
            {
                match tokio::fs::read(&profile_path).await {
                    Ok(bytes) => attachments.push((kind.filename(), bytes)),
                    Err(_) => omitted_profiles += 1,
                }
            } else {
                omitted_profiles += 1;
            }
        }
    }
    let mut context = context;
    context["profiles"] = serde_json::to_value(profile_status)?;
    context["omitted_profiles"] = serde_json::json!(omitted_profiles);
    context["resource_history_retention_hours"] = serde_json::json!(24);
    tokio::task::spawn_blocking(move || {
        write_archive(snapshot, context, &destination, attachments)
    })
    .await??;
    Ok(())
}
fn sanitize_description(text: &str) -> String {
    let mut redact_next = false;
    text.split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let stripped = word.trim_start_matches(['\"', '\'', '(', '[', '{']);
            let key = [
                "password",
                "token",
                "api_key",
                "apikey",
                "api-key",
                "secret",
                "authorization",
                "bearer",
            ]
            .iter()
            .any(|key| lower.contains(key));
            if key {
                redact_next = true;
                return "[removed]";
            }
            if redact_next {
                if word.chars().any(char::is_alphanumeric) {
                    redact_next = false;
                }
                return "[removed]";
            }
            let drive = stripped.as_bytes();
            if word.contains("://")
                || stripped.starts_with('/')
                || stripped.starts_with('\\')
                || stripped.starts_with("~/")
                || (drive.len() >= 3
                    && drive[0].is_ascii_alphabetic()
                    && drive[1] == b':'
                    && matches!(drive[2], b'\\' | b'/'))
            {
                "[removed]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn bounded_json(rows: Vec<serde_json::Value>) -> anyhow::Result<(Vec<u8>, usize)> {
    let total = rows.len();
    let mut kept = Vec::new();
    let mut bytes = 2;
    for row in rows {
        let size = serde_json::to_vec(&row)?.len() + 1;
        if bytes + size > 2 * 1024 * 1024 {
            break;
        }
        bytes += size;
        kept.push(row);
    }
    let omitted = total - kept.len();
    Ok((serde_json::to_vec(&kept)?, omitted))
}
struct BoundedFile(std::fs::File);
impl std::io::Write for BoundedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::io::Seek;
        if self.0.stream_position()?.saturating_add(buf.len() as u64) > 16 * 1024 * 1024 {
            return Err(std::io::Error::other("diagnostic archive exceeds limit"));
        }
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
impl std::io::Seek for BoundedFile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}
fn write_archive(
    snapshot: DiagnosticSnapshot,
    context: serde_json::Value,
    destination: &Path,
    attachments: Vec<(&str, Vec<u8>)>,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut temp = destination.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let result = (|| -> anyhow::Result<()> {
        let (incidents, incidents_omitted) = bounded_json(snapshot.incidents)?;
        let (transitions, transitions_omitted) = bounded_json(snapshot.transitions)?;
        let (performance, performance_omitted) = bounded_json(snapshot.performance)?;
        let manifest = serde_json::json!({"schema_version":1,"version":env!("CARGO_PKG_VERSION"),"created_at_unix_seconds":super::now(),
            "dropped_events":snapshot.dropped,"reported_lost_events":snapshot.reported_lost,"storage_gaps":snapshot.storage_gaps,"expired_or_evicted_records":snapshot.evicted,"omitted_records":incidents_omitted+transitions_omitted+performance_omitted,
            "retention_days":14,"local_only":true});
        let mut file_options = std::fs::OpenOptions::new();
        file_options
            .write(true)
            .read(true)
            .create(true)
            .truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        let mut zip = zip::ZipWriter::new(BoundedFile(file_options.open(&temp)?));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in [
            ("manifest.json", serde_json::to_vec(&manifest)?),
            ("incidents.json", incidents),
            ("transitions.json", transitions),
            ("performance.json", performance),
            ("context.json", serde_json::to_vec(&context)?),
        ] {
            zip.start_file(name, options)?;
            zip.write_all(&data)?;
        }
        for (name, bytes) in attachments {
            anyhow::ensure!(
                matches!(
                    name,
                    "runtime.json" | "cpu-profile.svg" | "heap-profile.json"
                ),
                "unexpected diagnostic attachment"
            );
            anyhow::ensure!(
                bytes.len() <= 4 * 1024 * 1024,
                "diagnostic attachment exceeds limit"
            );
            zip.start_file(name, options)?;
            zip.write_all(&bytes)?;
        }
        zip.finish()?.0.sync_all()?;
        std::fs::rename(&temp, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_and_profiles_are_allowlisted_and_bounded() {
        let dir =
            std::env::temp_dir().join(format!("musicata-bundle-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("bundle.zip");
        write_archive(
            DiagnosticSnapshot::default(),
            serde_json::json!({}),
            &dest,
            vec![
                ("runtime.json", b"{}".to_vec()),
                ("cpu-profile.svg", b"<svg/>".to_vec()),
            ],
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        assert!(zip.by_name("runtime.json").is_ok());
        assert!(zip.by_name("cpu-profile.svg").is_ok());
        assert!(
            write_archive(
                DiagnosticSnapshot::default(),
                serde_json::json!({}),
                &dest,
                vec![("private.db", vec![])]
            )
            .is_err()
        );
        assert!(
            write_archive(
                DiagnosticSnapshot::default(),
                serde_json::json!({}),
                &dest,
                vec![("runtime.json", vec![0; 4 * 1024 * 1024 + 1])]
            )
            .is_err()
        );
        assert!(dest.exists()); // Failed exports preserve the last completed bundle.
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn descriptions_remove_recognizable_secret_and_path_variants() {
        let text = r#"Password=secret password:secret api_key=secret C:\Users\Alice\Music
            {"password":"secret"} TOKEN: secret https://Alice:secret@example.test/x"#;
        let result = sanitize_description(text);
        assert!(!result.contains("secret"), "{result}");
        assert!(!result.contains("Alice"), "{result}");
    }
    #[test]
    fn bundle_contains_only_diagnostic_entries_and_reports_omissions() {
        let destination = std::env::temp_dir().join(format!(
            "musicata-diag-export-{}.zip",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut snapshot = DiagnosticSnapshot::default();
        snapshot.incidents =
            vec![serde_json::json!({"message":"connection refused","category":"mpd.command"})];
        write_archive(
            snapshot,
            serde_json::json!({"version":"test"}),
            &destination,
            vec![],
        )
        .unwrap();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(destination).unwrap()).unwrap();
        assert_eq!(archive.len(), 5);
        assert!(archive.by_name("manifest.json").is_ok());
        assert!(archive.by_name("musicata.db").is_err());
        let manifest: serde_json::Value =
            serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["omitted_records"], 0);
    }
}
