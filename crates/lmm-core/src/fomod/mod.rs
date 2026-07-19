//! FOMOD installer support: detection, parsing, dependency evaluation,
//! the interactive selection session, and plan building.
//!
//! See docs/FOMOD.md for the feature matrix and the integration design.
//! The one-paragraph version: everything in this module is pure with
//! respect to the game directory — a FOMOD install ends in an
//! [`plan::InstallPlan`] that the ordinary staging/deployment pipeline
//! executes. The interactive UI lives in the CLI crate; this module only
//! exposes the state machine it renders.

pub mod cond;
pub mod env;
pub mod model;
pub mod parse;
pub mod plan;
pub mod session;
pub mod store;

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};

pub use model::{Module, ModuleInfo};

/// A FOMOD installer located inside an extracted archive tree.
#[derive(Debug)]
pub struct Detected {
    /// The directory the installer's source paths are relative to: the
    /// parent of the `fomod/` directory.
    pub installer_root: PathBuf,
    pub config_path: PathBuf,
    pub info_path: Option<PathBuf>,
}

/// Search an extracted archive for `fomod/ModuleConfig.xml`, matching
/// directory and file names case-insensitively (archives are inconsistent
/// about `fomod` vs `Fomod` vs `FOMOD`). Descends a bounded number of
/// wrapper levels, breadth-first with sorted entries, so the result is
/// deterministic; the shallowest match wins.
pub fn detect(extracted: &Path) -> Result<Option<Detected>> {
    let mut level: Vec<PathBuf> = vec![extracted.to_path_buf()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for dir in &level {
            let mut subdirs = Vec::new();
            for entry in fs::read_dir(dir).path_ctx(dir)? {
                let entry = entry.path_ctx(dir)?;
                if entry.file_type().path_ctx(dir)?.is_dir() {
                    subdirs.push(entry.path());
                }
            }
            subdirs.sort();
            for sub in subdirs {
                if sub
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("fomod"))
                    && let Some(config) = file_ci(&sub, "moduleconfig.xml")?
                {
                    return Ok(Some(Detected {
                        installer_root: dir.clone(),
                        config_path: config,
                        info_path: file_ci(&sub, "info.xml")?,
                    }));
                }
                next.push(sub);
            }
        }
        level = next;
        if level.is_empty() {
            break;
        }
    }
    Ok(None)
}

/// Direct child file of `dir` with the given lowercase name.
fn file_ci(dir: &Path, name_lower: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir).path_ctx(dir)? {
        let entry = entry.path_ctx(dir)?;
        if entry.file_type().path_ctx(dir)?.is_file()
            && entry.file_name().to_string_lossy().to_lowercase() == name_lower
        {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Parse a detected installer's XML files. `info.xml` problems degrade to
/// empty metadata; `ModuleConfig.xml` problems are errors.
pub fn load(det: &Detected) -> Result<(Module, ModuleInfo)> {
    let config_xml = read_xml(&det.config_path)?;
    let mut module = parse::parse_module_config(&config_xml)?;

    let info = match &det.info_path {
        Some(p) => match read_xml(p).and_then(|xml| parse::parse_info(&xml)) {
            Ok(info) => info,
            Err(e) => {
                module.warnings.push(format!("info.xml unreadable: {e}"));
                ModuleInfo::default()
            }
        },
        None => ModuleInfo::default(),
    };
    if module.name == "(unnamed module)"
        && let Some(name) = &info.name
    {
        module.name = name.clone();
    }
    Ok((module, info))
}

/// Read an installer XML file with the size cap, tolerating the byte-order
/// marks and UTF-16 encodings Windows tools produce.
fn read_xml(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).path_ctx(path)?;
    if meta.len() > parse::LIMITS.max_xml_bytes {
        return Err(Error::UnsafeArchive(format!(
            "{}: larger than the {} MiB XML limit",
            path.display(),
            parse::LIMITS.max_xml_bytes / (1024 * 1024)
        )));
    }
    let bytes = fs::read(path).path_ctx(path)?;
    decode_xml_bytes(&bytes).ok_or_else(|| {
        Error::Fomod(format!(
            "{}: not valid UTF-8 or UTF-16 text",
            path.display()
        ))
    })
}

fn decode_xml_bytes(bytes: &[u8]) -> Option<String> {
    fn utf16(bytes: &[u8], le: bool) -> Option<String> {
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16(&units).ok()
    }
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8(bytes.to_vec()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_handles_boms() {
        assert_eq!(decode_xml_bytes(b"<a/>").unwrap(), "<a/>");
        assert_eq!(decode_xml_bytes(b"\xEF\xBB\xBF<a/>").unwrap(), "<a/>");
        let utf16le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("<a/>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_xml_bytes(&utf16le).unwrap(), "<a/>");
        assert!(decode_xml_bytes(b"\xff\xff\xfe").is_none());
    }
}
