//! The [`Environment`] implementation backed by a real installation: the
//! game adapter side of dependency evaluation.
//!
//! What can and cannot be answered on Linux/Proton:
//! * `file_state` — answerable. A file is *Active* when the game would see
//!   it: present in the game's mod-root directory (vanilla or deployed) or
//!   provided by an enabled mod of the active profile (lmm deploys enabled
//!   mods, so "enabled" is the user's intent even before the next deploy).
//!   It is *Inactive* when some installed mod provides it but none of the
//!   providers is enabled. Otherwise *Missing*.
//! * `game_version` — not answerable: Proton games carry their version in
//!   Windows PE resources lmm does not parse. Returns `None`, which the
//!   evaluator reports as an explicit limitation instead of a guess.
//! * `script_extender_version` — same story.

use std::path::PathBuf;

use rusqlite::OptionalExtension;

use crate::db::Db;
use crate::error::Result;
use crate::games::GameDef;
use crate::model::Installation;
use crate::paths::RelPath;

use super::cond::{Environment, Version};
use super::model::FileState;

pub struct InstallEnvironment<'a> {
    db: &'a Db,
    profile_id: i64,
    /// `<game root>/<mod_root>`: where the game looks for plugin files.
    mod_root_dir: PathBuf,
}

impl<'a> InstallEnvironment<'a> {
    pub fn new(
        db: &'a Db,
        inst: &Installation,
        game: &GameDef,
        profile_id: i64,
    ) -> InstallEnvironment<'a> {
        InstallEnvironment {
            db,
            profile_id,
            mod_root_dir: inst.path.join(game.mod_root),
        }
    }
}

impl Environment for InstallEnvironment<'_> {
    fn file_state(&self, file: &str) -> Result<FileState> {
        // Installer-supplied path: validate before it touches a filesystem.
        let rel = RelPath::parse(file)?;
        if rel.to_native(&self.mod_root_dir).exists() {
            return Ok(FileState::Active);
        }
        // enabled = MAX over providers: any enabled provider counts.
        let enabled: Option<bool> = self
            .db
            .conn
            .query_row(
                "SELECT MAX(pm.enabled) FROM mod_files f
                 JOIN profile_mods pm ON pm.mod_id = f.mod_id
                 WHERE pm.profile_id = ?1 AND f.path_key = ?2",
                rusqlite::params![self.profile_id, rel.key()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(match enabled {
            Some(true) => FileState::Active,
            Some(false) => FileState::Inactive,
            None => FileState::Missing,
        })
    }

    fn game_version(&self) -> Result<Option<Version>> {
        Ok(None)
    }

    fn script_extender_version(&self) -> Result<Option<Version>> {
        Ok(None)
    }
}
