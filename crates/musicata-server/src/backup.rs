//! Library export / import: a single zip holding a consistent database snapshot
//! (`musicata.db`) plus the acquired-artwork cache (`artwork/…`). This is Musicata's *state*
//! for migrating to another machine or backing up — not the music files themselves, which
//! live in the user's sources.
//!
//! Import is staged, not applied live: the upload is unpacked next to the database as
//! `<db>.import` / `<artwork>.import`, and [`apply_staged_import`] swaps it in at the next
//! startup (before the DB is opened) so we never replace a database the server has open.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;
use zip::write::SimpleFileOptions;

/// A completed export available for download.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExportInfo {
    pub name: String,
    pub size_bytes: u64,
    pub created_at_unix_seconds: i64,
}

/// Shared state for the (single) export job.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExportStatus {
    pub running: bool,
    pub latest: Option<ExportInfo>,
    pub error: Option<String>,
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Staging path the import is unpacked to, applied on next startup.
pub fn staged_db(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".import")
}
pub fn staged_artwork(artwork_dir: &Path) -> PathBuf {
    with_suffix(artwork_dir, ".import")
}

/// Build the export zip: the DB snapshot as `musicata.db`, the artwork tree under `artwork/`.
pub fn create_archive(db_snapshot: &Path, artwork_dir: &Path, dest_zip: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::create(dest_zip)
        .with_context(|| format!("create {}", dest_zip.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("musicata.db", options)?;
    let mut db = std::fs::File::open(db_snapshot)?;
    std::io::copy(&mut db, &mut zip)?;

    if artwork_dir.exists() {
        add_dir(&mut zip, artwork_dir, artwork_dir, options)?;
    }
    zip.finish()?;
    Ok(())
}

fn add_dir<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    base: &Path,
    dir: &Path,
    options: SimpleFileOptions,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            add_dir(zip, base, &path, options)?;
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let name = format!("artwork/{}", rel.to_string_lossy().replace('\\', "/"));
            zip.start_file(name, options)?;
            let mut f = std::fs::File::open(&path)?;
            std::io::copy(&mut f, zip)?;
        }
    }
    Ok(())
}

/// Unpack an uploaded export zip into the staging paths. Replaces any prior staging.
pub fn extract_import(zip_bytes: &[u8], db_import: &Path, artwork_import: &Path) -> anyhow::Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .context("not a valid zip archive")?;

    let _ = std::fs::remove_file(db_import);
    let _ = std::fs::remove_dir_all(artwork_import);
    let mut found_db = false;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name == "musicata.db" {
            let mut out = std::fs::File::create(db_import)?;
            copy(&mut entry, &mut out)?;
            found_db = true;
        } else if let Some(rel) = name.strip_prefix("artwork/") {
            if entry.is_dir() || rel.is_empty() {
                continue;
            }
            let dest = artwork_import.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&dest)?;
            copy(&mut entry, &mut out)?;
        }
    }
    if !found_db {
        bail!("archive does not contain a Musicata database (musicata.db)");
    }
    Ok(())
}

fn copy(reader: &mut impl Read, writer: &mut impl Write) -> anyhow::Result<()> {
    std::io::copy(reader, writer)?;
    Ok(())
}

/// At startup, before the database is opened: if a staged import is present, swap it over the
/// live database + artwork. Safe because nothing has the DB open yet.
pub fn apply_staged_import(db_path: &Path, artwork_dir: &Path) {
    let db_import = staged_db(db_path);
    if db_import.exists() {
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(with_suffix(db_path, "-wal"));
        let _ = std::fs::remove_file(with_suffix(db_path, "-shm"));
        match std::fs::rename(&db_import, db_path) {
            Ok(()) => tracing::info!("applied imported library database"),
            Err(error) => tracing::error!(%error, "failed to apply imported database"),
        }
    }
    let art_import = staged_artwork(artwork_dir);
    if art_import.exists() {
        let _ = std::fs::remove_dir_all(artwork_dir);
        if let Err(error) = std::fs::rename(&art_import, artwork_dir) {
            tracing::error!(%error, "failed to apply imported artwork");
        }
    }
}
