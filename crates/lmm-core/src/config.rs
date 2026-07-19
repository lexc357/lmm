use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

/// User configuration, loaded from TOML. Every field has a default so an
/// absent or empty config file is valid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub general: General,
    pub discovery: Discovery,
    pub limits: Limits,
    pub shell: Shell,
    pub fomod: Fomod,
    pub deploy: Deploy,
}

/// How staged files are placed into the game directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployMethod {
    /// Copy each staged file (temp + rename, hash-verified). Always works.
    #[default]
    Copy,
    /// Hard-link staged files into the game directory: instant and free of
    /// disk duplication, but only within one filesystem — files that cannot
    /// be linked are copied instead. Caveat: a game or tool that rewrites a
    /// linked file in place also rewrites the staged copy ('lmm verify'
    /// detects this as drift, but repair then needs the mod reinstalled).
    Hardlink,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Deploy {
    pub method: DeployMethod,
}

/// FOMOD installer behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Fomod {
    /// Command used by the installer's `open <n>` to display an option
    /// image (e.g. "xdg-open"). Unset = images are shown as paths only.
    /// Only this user-configured command is ever executed — never anything
    /// from a mod archive.
    pub image_viewer: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Shell {
    pub autocomplete: Autocomplete,
}

/// Interactive-shell completion behavior. All on by default; the shell
/// degrades gracefully when features are turned off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Autocomplete {
    /// Master switch for tab completion and the candidate menu.
    pub enabled: bool,
    /// Faded inline suggestion after the cursor while typing.
    pub inline_suggestion: bool,
    /// Allow subsequence (fuzzy) matches after the more precise tiers.
    pub fuzzy_matching: bool,
    /// Show short descriptions (e.g. enabled/disabled) next to candidates.
    pub show_descriptions: bool,
}

impl Default for Autocomplete {
    fn default() -> Self {
        Autocomplete {
            enabled: true,
            inline_suggestion: true,
            fuzzy_matching: true,
            show_descriptions: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    /// Overrides the default XDG data directory (~/.local/share/lmm).
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Discovery {
    /// Additional Steam root directories to scan besides the well-known ones.
    pub extra_steam_roots: Vec<PathBuf>,
}

/// Bounds applied while validating and extracting untrusted archives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Maximum number of entries in an archive.
    pub max_archive_entries: u64,
    /// Maximum uncompressed size of a single file (MiB).
    pub max_file_size_mib: u64,
    /// Maximum total uncompressed size of an archive (MiB).
    pub max_total_size_mib: u64,
    /// Reject archives whose total uncompressed/compressed ratio exceeds this
    /// (only applied above a small floor so tiny highly-compressible files pass).
    pub max_compression_ratio: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_archive_entries: 200_000,
            max_file_size_mib: 8 * 1024,
            max_total_size_mib: 64 * 1024,
            max_compression_ratio: 300,
        }
    }
}

impl Limits {
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size_mib * 1024 * 1024
    }
    pub fn max_total_size(&self) -> u64 {
        self.max_total_size_mib * 1024 * 1024
    }
}

impl Config {
    /// Load config from `path`. A missing file yields the default config;
    /// a malformed file is an error (silently ignoring user config is worse).
    pub fn load(path: &Path) -> Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }
}

/// Resolved filesystem locations for this run.
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub staging_dir: PathBuf,
    pub backups_dir: PathBuf,
    /// Extraction scratch. Lives inside the data dir so that moving validated
    /// trees into staging is a same-filesystem rename, not a copy.
    pub tmp_dir: PathBuf,
    /// Completed Nexus download archives land here.
    pub downloads_dir: PathBuf,
    /// nxm:// requests received while no lmm instance was running wait here
    /// (one small file per request) until the next start drains them.
    pub spool_dir: PathBuf,
    /// Standalone Game Tools installs: tools/<installation_id>/<tool_id>/.
    pub tools_dir: PathBuf,
    /// Automatic plugins.txt backups: loadorder/<installation_id>/.
    pub loadorder_dir: PathBuf,
}

impl DataPaths {
    pub fn new(data_dir: PathBuf, db_override: Option<PathBuf>) -> DataPaths {
        DataPaths {
            db_path: db_override.unwrap_or_else(|| data_dir.join("lmm.db")),
            staging_dir: data_dir.join("staging"),
            backups_dir: data_dir.join("backups"),
            tmp_dir: data_dir.join("tmp"),
            downloads_dir: data_dir.join("downloads"),
            spool_dir: data_dir.join("nxm-spool"),
            tools_dir: data_dir.join("tools"),
            loadorder_dir: data_dir.join("loadorder"),
            data_dir,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.data_dir,
            &self.staging_dir,
            &self.backups_dir,
            &self.tmp_dir,
            &self.downloads_dir,
            &self.spool_dir,
            &self.tools_dir,
            &self.loadorder_dir,
        ] {
            fs::create_dir_all(dir).path_ctx(dir)?;
        }
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent).path_ctx(parent)?;
        }
        Ok(())
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Config("HOME is not set".into()))
}

/// XDG config file location: $XDG_CONFIG_HOME/lmm/config.toml.
pub fn default_config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir()?.join(".config"),
    };
    Ok(base.join("lmm").join("config.toml"))
}

/// XDG data dir: $XDG_DATA_HOME/lmm.
pub fn default_data_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir()?.join(".local").join("share"),
    };
    Ok(base.join("lmm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default() {
        let cfg = Config::load(Path::new("/nonexistent/lmm-config.toml")).unwrap();
        assert_eq!(cfg.limits.max_compression_ratio, 300);
    }

    #[test]
    fn unknown_keys_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        fs::write(&p, "[general]\ntypo_key = 1\n").unwrap();
        assert!(Config::load(&p).is_err());
    }

    #[test]
    fn deploy_method_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        fs::write(&p, "[deploy]\nmethod = \"hardlink\"\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.deploy.method, DeployMethod::Hardlink);
        assert_eq!(Config::default().deploy.method, DeployMethod::Copy);

        fs::write(&p, "[deploy]\nmethod = \"symlink\"\n").unwrap();
        assert!(Config::load(&p).is_err(), "unknown methods are rejected");
    }

    #[test]
    fn partial_config_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        fs::write(&p, "[limits]\nmax_archive_entries = 5\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.limits.max_archive_entries, 5);
        assert_eq!(cfg.limits.max_compression_ratio, 300);
    }
}
