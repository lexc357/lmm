//! lmm-core: the mod-management engine behind the `lmm` CLI.
//!
//! Interface-agnostic by design: nothing here prints, prompts, or exits.
//! Destructive operations are split into plan (pure) and execute (takes the
//! plan) so any frontend can implement dry-run and confirmation.
//! See docs/DESIGN.md for the architecture.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod archive;
pub mod config;
pub mod db;
pub mod deploy;
pub mod discovery;
pub mod error;
pub mod fomod;
pub mod games;
pub mod hash;
pub mod installs;
pub mod launch;
pub mod model;
pub mod mods;
pub mod paths;
pub mod profile;
pub mod resolve;
pub mod staging;
pub mod tools;
pub mod verify;

use std::path::PathBuf;

use config::{Config, DataPaths};
use db::Db;
use error::Result;

/// Everything a frontend needs to run lmm operations.
pub struct Context {
    pub config: Config,
    pub paths: DataPaths,
    pub db: Db,
}

/// CLI-provided overrides for file locations.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub config_path: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub db_path: Option<PathBuf>,
}

impl Context {
    pub fn open(overrides: &Overrides) -> Result<Context> {
        let config_path = match &overrides.config_path {
            Some(p) => p.clone(),
            None => config::default_config_path()?,
        };
        let config = Config::load(&config_path)?;

        // Precedence for the data dir: CLI flag > config file > XDG default.
        let data_dir = overrides
            .data_dir
            .clone()
            .or_else(|| config.general.data_dir.clone())
            .map_or_else(config::default_data_dir, Ok)?;

        let paths = DataPaths::new(data_dir, overrides.db_path.clone());
        paths.ensure_dirs()?;

        let db = Db::open(&paths.db_path)?;
        games::sync_registry(&db)?;

        Ok(Context { config, paths, db })
    }
}
