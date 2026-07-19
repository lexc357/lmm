//! ZIP extraction. Entry names are validated by the shared `Extractor`;
//! this module only handles format specifics.

use std::fs::File;
use std::path::Path;

use crate::error::{Error, IoContext, Result};

use super::Extractor;

/// Unix file-type bits (high 4 bits of the mode): symlink.
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

pub(super) fn extract(path: &Path, ext: &mut Extractor) -> Result<()> {
    let file = File::open(path).path_ctx(path)?;
    let mut zip = ::zip::ZipArchive::new(file)
        .map_err(|e| Error::Archive(format!("{}: invalid zip: {e}", path.display())))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::Archive(format!("zip entry {i}: {e}")))?;

        if entry.encrypted() {
            return Err(Error::Archive(
                "password-protected archives are not supported".into(),
            ));
        }
        // Symlink entries would let a later entry write through the link to
        // an arbitrary location; reject the whole archive.
        if let Some(mode) = entry.unix_mode()
            && mode & S_IFMT == S_IFLNK
        {
            return Err(Error::UnsafeArchive(format!(
                "symlink entry '{}'",
                entry.name()
            )));
        }

        let is_dir = entry.is_dir();
        // Use the raw name and do our own validation; do not trust the
        // crate's lossy sanitization to match our policy.
        let raw_name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        if let Some(target) = ext.begin_entry(&raw_name, is_dir)? {
            ext.write_entry(&target, &mut entry)?;
        }
    }
    Ok(())
}
