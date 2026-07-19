//! Persistence of FOMOD installations: one `fomod_installs` row per mod,
//! holding everything needed to show, replay, or re-open the choices —
//! module name, the ModuleConfig hash the choices were made against, the
//! selections (with flags), and the final normalized file plan.
//!
//! The row cascades away with its mod. Plans and selections are stored as
//! JSON; on load the plan's paths pass through RelPath's validating
//! deserializer, so a tampered database cannot smuggle hostile paths back
//! into the pipeline.

use rusqlite::{OptionalExtension, params};

use crate::db::{Db, now};
use crate::error::{Error, Result};

use super::plan::PlannedFile;
use super::session::Selections;

/// Format tag stored with every record, bumped if interpretation changes.
pub const FORMAT: &str = "fomod-xml/1";

#[derive(Debug)]
pub struct FomodRecord {
    pub mod_id: i64,
    pub module_name: String,
    /// SHA-256 of the ModuleConfig.xml bytes the user configured against.
    pub config_sha256: String,
    pub format: String,
    pub selections: Selections,
    pub plan: Vec<PlannedFile>,
    pub created_at: i64,
}

/// Insert or replace the record inside the caller's transaction scope.
pub fn save(
    conn: &rusqlite::Connection,
    mod_id: i64,
    module_name: &str,
    config_sha256: &str,
    selections: &Selections,
    plan: &[PlannedFile],
) -> Result<()> {
    let choices_json = serde_json::to_string(selections)
        .map_err(|e| Error::Invalid(format!("encoding fomod choices: {e}")))?;
    let plan_json = serde_json::to_string(plan)
        .map_err(|e| Error::Invalid(format!("encoding fomod plan: {e}")))?;
    conn.execute(
        "INSERT INTO fomod_installs
             (mod_id, module_name, config_sha256, format, choices_json, plan_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(mod_id) DO UPDATE SET
             module_name = excluded.module_name,
             config_sha256 = excluded.config_sha256,
             format = excluded.format,
             choices_json = excluded.choices_json,
             plan_json = excluded.plan_json,
             created_at = excluded.created_at",
        params![
            mod_id,
            module_name,
            config_sha256,
            FORMAT,
            choices_json,
            plan_json,
            now(),
        ],
    )?;
    Ok(())
}

/// Locate an archive with the given hash via the download store (Nexus
/// downloads record their path and hash on completion).
pub fn find_archive_by_sha(db: &Db, sha256: &str) -> Result<Option<String>> {
    Ok(db
        .conn
        .query_row(
            "SELECT archive_path FROM downloads
             WHERE sha256 = ?1 AND archive_path IS NOT NULL",
            [sha256],
            |r| r.get(0),
        )
        .optional()?)
}

/// The FOMOD record for a mod, if it was installed through the installer.
pub fn get(db: &Db, mod_id: i64) -> Result<Option<FomodRecord>> {
    let row = db
        .conn
        .query_row(
            "SELECT module_name, config_sha256, format, choices_json, plan_json, created_at
             FROM fomod_installs WHERE mod_id = ?1",
            [mod_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((module_name, config_sha256, format, choices_json, plan_json, created_at)) = row
    else {
        return Ok(None);
    };
    if format != FORMAT {
        return Err(Error::Invalid(format!(
            "mod {mod_id}: fomod record format '{format}' is not supported by this lmm"
        )));
    }
    let selections: Selections = serde_json::from_str(&choices_json)
        .map_err(|e| Error::Invalid(format!("mod {mod_id}: corrupt fomod choices: {e}")))?;
    let plan: Vec<PlannedFile> = serde_json::from_str(&plan_json)
        .map_err(|e| Error::Invalid(format!("mod {mod_id}: corrupt fomod plan: {e}")))?;
    Ok(Some(FomodRecord {
        mod_id,
        module_name,
        config_sha256,
        format,
        selections,
        plan,
        created_at,
    }))
}
