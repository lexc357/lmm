//! `RelPath`: the only path type allowed to address files inside a game
//! directory, a staging directory, or an archive.
//!
//! Every path that originates from untrusted input (archive entries, database
//! rows, user arguments) is funneled through `RelPath::parse`, which rejects
//! everything that could escape a base directory. Code that joins a `RelPath`
//! onto a base can rely on the result staying under that base (absent
//! symlinks, which deployment checks separately).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// Longest path we accept, matching Linux PATH_MAX conventions.
const MAX_PATH_LEN: usize = 4096;
/// Longest single component (ext4/btrfs/xfs limit is 255 bytes).
const MAX_COMPONENT_LEN: usize = 255;

/// A validated, normalized, '/'-separated relative path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RelPath(String);

/// Deserializing re-validates: a RelPath loaded from the database or a
/// JSON plan is as trustworthy as one parsed from an archive entry.
impl<'de> serde::Deserialize<'de> for RelPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        RelPath::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl RelPath {
    /// Parse and validate an untrusted path string (e.g. an archive entry
    /// name). Windows separators are normalized; anything that is absolute,
    /// escapes upward, or abuses the filesystem is rejected.
    pub fn parse(raw: &str) -> Result<RelPath> {
        let unsafe_path = |why: &str| Error::UnsafeArchive(format!("path '{raw}': {why}"));

        if raw.len() > MAX_PATH_LEN {
            return Err(unsafe_path("path too long"));
        }
        if raw.bytes().any(|b| b == 0 || b < 0x20) {
            return Err(unsafe_path("control character in path"));
        }
        // Zip entries from Windows tools may use backslashes.
        let norm = raw.replace('\\', "/");
        if norm.starts_with('/') {
            return Err(unsafe_path("absolute path"));
        }
        // Drive-letter ("C:...") or other scheme-like prefixes.
        if let Some(first) = norm.split('/').next()
            && first.contains(':')
        {
            return Err(unsafe_path("drive letter or colon in first component"));
        }

        let mut parts: Vec<&str> = Vec::new();
        for comp in norm.split('/') {
            match comp {
                // Collapse harmless artifacts ("a//b", "./a", trailing '/').
                "" | "." => continue,
                ".." => return Err(unsafe_path("parent-directory traversal")),
                c if c.len() > MAX_COMPONENT_LEN => {
                    return Err(unsafe_path("component too long"));
                }
                c => parts.push(c),
            }
        }
        if parts.is_empty() {
            return Err(unsafe_path("empty path"));
        }
        Ok(RelPath(parts.join("/")))
    }

    /// Build from an on-disk path relative to a trusted base (staging walk).
    /// Still fully validated: defense in depth against odd filenames.
    pub fn from_os_rel(rel: &Path) -> Result<RelPath> {
        let s = rel
            .to_str()
            .ok_or_else(|| Error::Invalid(format!("non-UTF-8 filename: {}", rel.display())))?;
        RelPath::parse(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Case-insensitive identity: conflict and ownership key. Windows games
    /// under Proton resolve paths case-insensitively, so `Textures/a.dds`
    /// and `textures/A.dds` are the same file to the game.
    pub fn key(&self) -> String {
        self.0.to_lowercase()
    }

    /// Join onto a base directory. Safe by construction: no component of a
    /// parsed RelPath can point upward or to an absolute location.
    pub fn to_native(&self, base: &Path) -> PathBuf {
        let mut p = base.to_path_buf();
        for comp in self.0.split('/') {
            p.push(comp);
        }
        p
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Parent directory path within the same base, if any.
    pub fn parent(&self) -> Option<RelPath> {
        self.0.rsplit_once('/').map(|(dir, _)| RelPath(dir.into()))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        for (input, expect) in [
            ("Data/Textures/a.dds", "Data/Textures/a.dds"),
            ("a\\b\\c.txt", "a/b/c.txt"),
            ("./a/b", "a/b"),
            ("a//b/", "a/b"),
            ("meshes/actor.nif", "meshes/actor.nif"),
        ] {
            assert_eq!(RelPath::parse(input).unwrap().as_str(), expect, "{input}");
        }
    }

    #[test]
    fn rejects_hostile_paths() {
        for input in [
            "../etc/passwd",
            "a/../../b",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "C:\\Windows\\system32.dll",
            "c:boot.ini",
            "a/..",
            "..",
            "",
            ".",
            "a/./../b",
            "file\x00name",
            "eviL\n.txt",
        ] {
            assert!(RelPath::parse(input).is_err(), "should reject: {input:?}");
        }
    }

    #[test]
    fn key_is_case_insensitive() {
        let a = RelPath::parse("Data/Textures/A.DDS").unwrap();
        let b = RelPath::parse("data/textures/a.dds").unwrap();
        assert_eq!(a.key(), b.key());
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn to_native_stays_under_base() {
        let rp = RelPath::parse("a/b/c.txt").unwrap();
        let joined = rp.to_native(Path::new("/base"));
        assert_eq!(joined, PathBuf::from("/base/a/b/c.txt"));
    }

    #[test]
    fn parent() {
        assert_eq!(
            RelPath::parse("a/b/c").unwrap().parent().unwrap().as_str(),
            "a/b"
        );
        assert!(RelPath::parse("a").unwrap().parent().is_none());
    }
}
