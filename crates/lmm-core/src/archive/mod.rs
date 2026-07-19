//! Archive import: enumeration, validation and bounded extraction.
//!
//! Archives are untrusted input. Nothing from an archive touches the game
//! directory directly: entries are validated (`RelPath`), extracted into a
//! fresh temporary directory with hard byte budgets, and only then inspected
//! and staged. See docs/DESIGN.md §8 for the threat model.

mod layout;
mod sevenz;
mod zip;

pub use layout::{DetectedLayout, detect_mod_root};

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::Limits;
use crate::error::{Error, IoContext, Result};
use crate::paths::RelPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    SevenZ,
}

/// Identify the archive by magic bytes; extensions lie.
pub fn detect_kind(path: &Path) -> Result<Kind> {
    let mut file = File::open(path).path_ctx(path)?;
    let mut magic = [0u8; 8];
    let n = file.read(&mut magic).path_ctx(path)?;
    let magic = &magic[..n];
    if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
        return Ok(Kind::Zip);
    }
    if magic.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Ok(Kind::SevenZ);
    }
    if magic.starts_with(b"Rar!") {
        return Err(Error::Archive(
            "RAR archives are not supported yet; re-pack as .zip or .7z".into(),
        ));
    }
    Err(Error::Archive(format!(
        "{}: not a recognized archive (supported: .zip, .7z)",
        path.display()
    )))
}

/// Extract `archive` into the empty directory `dest`, enforcing limits.
/// Returns the number of files written.
pub fn extract(archive: &Path, dest: &Path, limits: &Limits) -> Result<u64> {
    let compressed_size = archive.metadata().path_ctx(archive)?.len();
    let mut ext = Extractor::new(dest, limits, compressed_size);
    match detect_kind(archive)? {
        Kind::Zip => zip::extract(archive, &mut ext)?,
        Kind::SevenZ => sevenz::extract(archive, &mut ext)?,
    }
    ext.finish()
}

/// Shared per-entry validation and budget enforcement for all formats.
///
/// The budgets are enforced on *actual bytes decompressed*, not on header
/// claims — a bomb with lying headers hits the same wall.
struct Extractor<'a> {
    dest: &'a Path,
    limits: &'a Limits,
    compressed_size: u64,
    total_written: u64,
    files_written: u64,
    entries_seen: u64,
    /// First-seen on-disk casing per case-insensitive key. A later entry that
    /// differs only in case overwrites the same file, mirroring how the game
    /// (case-insensitive under Proton) would see it.
    by_key: HashMap<String, RelPath>,
}

impl<'a> Extractor<'a> {
    fn new(dest: &'a Path, limits: &'a Limits, compressed_size: u64) -> Self {
        Extractor {
            dest,
            limits,
            compressed_size,
            total_written: 0,
            files_written: 0,
            entries_seen: 0,
            by_key: HashMap::new(),
        }
    }

    fn bomb(&self, why: String) -> Error {
        Error::UnsafeArchive(format!("{why} (raise [limits] in config if legitimate)"))
    }

    /// Validate an entry name and account for it. Returns the target path
    /// for file entries, or None for directories.
    fn begin_entry(&mut self, raw_name: &str, is_dir: bool) -> Result<Option<PathBuf>> {
        self.entries_seen += 1;
        if self.entries_seen > self.limits.max_archive_entries {
            return Err(self.bomb(format!(
                "more than {} entries",
                self.limits.max_archive_entries
            )));
        }
        let rel = RelPath::parse(raw_name)?;
        if is_dir {
            // Directories are only created as needed for files; an empty dir
            // carries no mod content.
            return Ok(None);
        }
        let rel = self.by_key.entry(rel.key()).or_insert(rel).clone();
        Ok(Some(rel.to_native(self.dest)))
    }

    /// Stream one entry's bytes to `target`, enforcing per-file and total
    /// budgets while decompressing.
    fn write_entry(&mut self, target: &Path, reader: &mut dyn Read) -> Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).path_ctx(parent)?;
        }
        let file = File::create(target).path_ctx(target)?;
        let mut writer = std::io::BufWriter::new(file);
        let mut buf = [0u8; 64 * 1024];
        let mut written: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| Error::Archive(format!("decompression failed: {e}")))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > self.limits.max_file_size() {
                return Err(self.bomb(format!(
                    "entry exceeds file size limit of {} MiB",
                    self.limits.max_file_size_mib
                )));
            }
            if self.total_written + written > self.limits.max_total_size() {
                return Err(self.bomb(format!(
                    "archive exceeds total size limit of {} MiB",
                    self.limits.max_total_size_mib
                )));
            }
            std::io::Write::write_all(&mut writer, &buf[..n]).path_ctx(target)?;
        }
        std::io::Write::flush(&mut writer).path_ctx(target)?;
        self.total_written += written;
        self.files_written += 1;

        // Ratio guard: catches "small archive, huge output" bombs early-ish;
        // the absolute caps above are the hard backstop. Floor of 64 MiB so
        // legitimately compressible small files never trip it.
        if self.total_written > 64 * 1024 * 1024
            && self.compressed_size > 0
            && self.total_written / self.compressed_size > self.limits.max_compression_ratio
        {
            return Err(self.bomb(format!(
                "compression ratio exceeds {}:1",
                self.limits.max_compression_ratio
            )));
        }
        Ok(())
    }

    fn finish(self) -> Result<u64> {
        if self.files_written == 0 {
            return Err(Error::Archive("archive contains no files".into()));
        }
        Ok(self.files_written)
    }
}
