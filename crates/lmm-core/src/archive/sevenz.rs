//! 7z extraction via sevenz-rust2. Names are validated by the shared
//! `Extractor`; this module handles format specifics.

use std::path::Path;

use sevenz_rust2::{ArchiveReader, Password};

use crate::error::{Error, Result};

use super::Extractor;

/// Windows attribute flag signalling "unix mode stored in the high 16 bits"
/// (set by 7-zip on Unix) and the reparse-point flag (Windows symlinks).
const ATTR_UNIX_EXTENSION: u32 = 0x8000;
const ATTR_REPARSE_POINT: u32 = 0x400;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

pub(super) fn extract(path: &Path, ext: &mut Extractor) -> Result<()> {
    let mut reader = ArchiveReader::open(path, Password::empty())
        .map_err(|e| Error::Archive(format!("{}: invalid 7z: {e}", path.display())))?;

    // First pass over metadata: reject symlinks/reparse points outright.
    for entry in reader.archive().files.iter() {
        if entry.has_windows_attributes {
            let attrs = entry.windows_attributes;
            if attrs & ATTR_REPARSE_POINT != 0 {
                return Err(Error::UnsafeArchive(format!(
                    "reparse-point/symlink entry '{}'",
                    entry.name
                )));
            }
            if attrs & ATTR_UNIX_EXTENSION != 0 && (attrs >> 16) & S_IFMT == S_IFLNK {
                return Err(Error::UnsafeArchive(format!(
                    "symlink entry '{}'",
                    entry.name
                )));
            }
        }
    }

    let mut result: Result<()> = Ok(());
    reader
        .for_each_entries(|entry, entry_reader| {
            // Empty-file entries can arrive with has_stream = false; they
            // still need creating, and the reader yields 0 bytes for them.
            let is_dir = entry.is_directory;
            match ext.begin_entry(&entry.name, is_dir) {
                Ok(Some(target)) => match ext.write_entry(&target, entry_reader) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        result = Err(e);
                        Ok(false) // stop iteration; error reported via `result`
                    }
                },
                Ok(None) => Ok(true),
                Err(e) => {
                    result = Err(e);
                    Ok(false)
                }
            }
        })
        .map_err(|e| Error::Archive(format!("7z extraction failed: {e}")))?;
    result
}
