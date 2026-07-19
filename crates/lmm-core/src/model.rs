//! Plain row types shared across the crate and serialized for `--json` output.

use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Installation {
    pub id: i64,
    pub game_slug: String,
    pub game_name: String,
    pub path: PathBuf,
    pub source: String,
    pub proton_prefix: Option<PathBuf>,
    pub label: Option<String>,
    pub active_profile_id: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub id: i64,
    pub installation_id: i64,
    pub name: String,
    pub is_active: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Mod {
    pub id: i64,
    pub installation_id: i64,
    pub name: String,
    pub version: Option<String>,
    pub archive_name: String,
    pub archive_sha256: String,
    pub staging_dir: String,
    pub installed_at: i64,
    pub file_count: i64,
}

/// Mod plus its state in a specific profile.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileMod {
    #[serde(flatten)]
    pub info: Mod,
    pub enabled: bool,
    pub priority: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModFile {
    pub mod_id: i64,
    pub rel_path: String,
    pub path_key: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployedFile {
    pub installation_id: i64,
    pub path_key: String,
    pub rel_path: String,
    pub provider_mod_id: i64,
    pub sha256: String,
    pub backup_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Backup {
    pub id: i64,
    pub installation_id: i64,
    pub path_key: String,
    pub rel_path: String,
    pub backup_path: String,
    pub sha256: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: i64,
    pub installation_id: i64,
    pub profile_id: Option<i64>,
    pub kind: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
