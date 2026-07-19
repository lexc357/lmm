//! Installed mods: import from archive, inventory, lookup, uninstall.

use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::db::{Db, now};
use crate::error::{Error, Result};
use crate::games;
use crate::model::{Installation, Mod, ModFile};
use crate::staging::{self, ImportedMod};
use crate::{Context, profile};

#[derive(Debug, Default)]
pub struct InstallOptions<'a> {
    pub name: Option<&'a str>,
    pub version: Option<&'a str>,
}

/// Outcome of an install, for frontend display.
#[derive(Debug)]
pub struct Installed {
    pub info: Mod,
    pub layout_rule: &'static str,
    pub layout_uncertain: bool,
}

/// An install after phase 1 (checks + extraction), before staging. The
/// frontend decides how to finish it: [`finish_plain`] for ordinary
/// archives, [`finish_fomod`] after running the installer. Dropping it
/// discards the extracted tree.
pub struct Prepared {
    pub name: String,
    pub version: Option<String>,
    pub archive_name: String,
    pub game: &'static crate::games::GameDef,
    pub extracted: staging::Extracted,
}

/// Metadata for recording a FOMOD install alongside the mod row.
pub struct FomodData<'a> {
    pub module_name: &'a str,
    pub config_sha256: &'a str,
    pub selections: &'a crate::fomod::session::Selections,
    pub plan: &'a [crate::fomod::plan::PlannedFile],
}

/// Phase 1: resolve the name, fail early on duplicates, hash and extract.
/// Nothing is written outside the scratch directory.
pub fn prepare(
    ctx: &Context,
    inst: &Installation,
    archive_path: &Path,
    opts: &InstallOptions,
) -> Result<Prepared> {
    let game = games::by_slug(&inst.game_slug)
        .ok_or_else(|| Error::Invalid(format!("unknown game slug '{}'", inst.game_slug)))?;

    let archive_name = archive_path
        .file_name()
        .ok_or_else(|| Error::Invalid(format!("{}: not a file", archive_path.display())))?
        .to_string_lossy()
        .into_owned();
    let name = match opts.name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        Some(_) => return Err(Error::Invalid("mod name cannot be empty".into())),
        None => default_name(&archive_name),
    };

    // Fail early on duplicates, before the (expensive) extraction.
    if find_by_name(&ctx.db, inst.id, &name)?.is_some() {
        return Err(Error::Invalid(format!(
            "a mod named '{name}' is already installed; pass --name to pick another"
        )));
    }

    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, archive_path)?;

    if let Some(existing) = find_by_archive(&ctx.db, inst.id, &extracted.archive_sha256)? {
        // Same bytes already installed: almost certainly a mistake.
        return Err(Error::Invalid(format!(
            "this exact archive is already installed as '{existing}'"
        )));
    }

    Ok(Prepared {
        name,
        version: opts.version.map(str::to_string),
        archive_name,
        game,
        extracted,
    })
}

/// Phase 2 for plain archives: layout detection, staging, recording.
pub fn finish_plain(ctx: &Context, inst: &Installation, prepared: &Prepared) -> Result<Installed> {
    let imported = staging::finish_import(&ctx.paths, prepared.game, &prepared.extracted)?;
    record_or_cleanup(ctx, inst, prepared, &imported, None)
}

/// Phase 2 for FOMOD installs: materialize the plan into staging and
/// record the mod together with its installer record, atomically.
pub fn finish_fomod(
    ctx: &Context,
    inst: &Installation,
    prepared: &Prepared,
    installer_root: &Path,
    fomod: &FomodData<'_>,
) -> Result<Installed> {
    let imported = staging::finish_import_planned(
        &ctx.paths,
        &prepared.extracted,
        fomod.plan,
        installer_root,
    )?;
    record_or_cleanup(ctx, inst, prepared, &imported, Some(fomod))
}

fn record_or_cleanup(
    ctx: &Context,
    inst: &Installation,
    prepared: &Prepared,
    imported: &ImportedMod,
    fomod: Option<&FomodData<'_>>,
) -> Result<Installed> {
    match record_install(
        ctx,
        inst,
        &prepared.name,
        prepared.version.as_deref(),
        &prepared.archive_name,
        imported,
        fomod,
    ) {
        Ok(mod_id) => Ok(Installed {
            info: get(&ctx.db, mod_id)?,
            layout_rule: imported.layout_rule,
            layout_uncertain: imported.layout_uncertain,
        }),
        Err(e) => {
            // The DB knows nothing about this mod; remove the staged files so
            // filesystem and database stay consistent.
            let _ = staging::remove_staged(&ctx.paths, &imported.staging_name);
            Err(e)
        }
    }
}

/// Install a mod archive for an installation: extract, validate, stage,
/// record. The mod starts disabled in every profile; nothing touches the
/// game directory until `deploy`. FOMOD archives are installed as-is by
/// this path — the interactive flow lives in the frontend, which calls
/// [`prepare`]/[`finish_fomod`] itself.
pub fn install(
    ctx: &Context,
    inst: &Installation,
    archive_path: &Path,
    opts: &InstallOptions,
) -> Result<Installed> {
    let prepared = prepare(ctx, inst, archive_path, opts)?;
    finish_plain(ctx, inst, &prepared)
}

/// One transaction: mod row, file inventory, a disabled profile_mods row
/// (at the end of the load order) in every profile of the installation,
/// and — for installer-driven mods — the FOMOD record.
fn record_install(
    ctx: &Context,
    inst: &Installation,
    name: &str,
    version: Option<&str>,
    archive_name: &str,
    imported: &ImportedMod,
    fomod: Option<&FomodData<'_>>,
) -> Result<i64> {
    let tx = ctx.db.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO mods (installation_id, name, version, archive_name, archive_sha256,
                           staging_dir, installed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            inst.id,
            name,
            version,
            archive_name,
            imported.archive_sha256,
            imported.staging_name,
            now(),
        ],
    )?;
    let mod_id = tx.last_insert_rowid();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO mod_files (mod_id, rel_path, path_key, size, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for f in &imported.files {
            stmt.execute(params![
                mod_id,
                f.rel.as_str(),
                f.rel.key(),
                f.size as i64,
                f.sha256
            ])?;
        }
    }

    // New mods join every profile disabled, after all existing mods.
    let mut stmt = tx.prepare(
        "INSERT INTO profile_mods (profile_id, mod_id, enabled, priority)
         SELECT p.id, ?1, 0,
                1 + COALESCE((SELECT MAX(pm.priority) FROM profile_mods pm
                              WHERE pm.profile_id = p.id), 0)
         FROM profiles p WHERE p.installation_id = ?2",
    )?;
    stmt.execute(params![mod_id, inst.id])?;
    drop(stmt);

    if let Some(f) = fomod {
        crate::fomod::store::save(
            &tx,
            mod_id,
            f.module_name,
            f.config_sha256,
            f.selections,
            f.plan,
        )?;
    }

    tx.commit()?;
    Ok(mod_id)
}

/// Replace an existing FOMOD-installed mod's files with a new plan
/// (reinstall / reconfigure). Refused while any of the mod's files are
/// deployed, mirroring [`uninstall`]. The old staging directory survives
/// until the database transaction commits, so a failure at any point
/// leaves the previous installation fully intact.
pub fn replace_fomod_install(
    ctx: &Context,
    inst: &Installation,
    mod_id: i64,
    extracted: &staging::Extracted,
    installer_root: &Path,
    fomod: &FomodData<'_>,
) -> Result<Installed> {
    let m = get(&ctx.db, mod_id)?;
    if m.installation_id != inst.id {
        return Err(Error::NotFound(format!("mod {mod_id}")));
    }
    let deployed: i64 = ctx.db.conn.query_row(
        "SELECT COUNT(*) FROM deployed_files WHERE provider_mod_id = ?1",
        [mod_id],
        |r| r.get(0),
    )?;
    if deployed > 0 {
        return Err(Error::Blocked(format!(
            "'{}' has {deployed} deployed files; disable it and run 'deploy' (or 'purge') first",
            m.name
        )));
    }

    let imported =
        staging::finish_import_planned(&ctx.paths, extracted, fomod.plan, installer_root)?;

    let commit = || -> Result<()> {
        let tx = ctx.db.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE mods SET staging_dir = ?1, archive_sha256 = ?2, installed_at = ?3
             WHERE id = ?4",
            params![
                imported.staging_name,
                imported.archive_sha256,
                now(),
                mod_id
            ],
        )?;
        tx.execute("DELETE FROM mod_files WHERE mod_id = ?1", [mod_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO mod_files (mod_id, rel_path, path_key, size, sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in &imported.files {
                stmt.execute(params![
                    mod_id,
                    f.rel.as_str(),
                    f.rel.key(),
                    f.size as i64,
                    f.sha256
                ])?;
            }
        }
        crate::fomod::store::save(
            &tx,
            mod_id,
            fomod.module_name,
            fomod.config_sha256,
            fomod.selections,
            fomod.plan,
        )?;
        tx.commit()?;
        Ok(())
    };

    match commit() {
        Ok(()) => {
            // Old staging only goes away after the database points at the
            // new one; a failure here leaves an orphan dir, never a mod
            // whose files are missing.
            let _ = staging::remove_staged(&ctx.paths, &m.staging_dir);
            Ok(Installed {
                info: get(&ctx.db, mod_id)?,
                layout_rule: imported.layout_rule,
                layout_uncertain: imported.layout_uncertain,
            })
        }
        Err(e) => {
            let _ = staging::remove_staged(&ctx.paths, &imported.staging_name);
            Err(e)
        }
    }
}

/// "SkyUI_5_2_SE-12604-5-2SE.zip" -> "SkyUI_5_2_SE-12604-5-2SE" is still ugly
/// but predictable; users can pass --name. We only strip the extension.
fn default_name(archive_name: &str) -> String {
    let lower = archive_name.to_lowercase();
    for ext in [".zip", ".7z"] {
        if lower.ends_with(ext) {
            return archive_name[..archive_name.len() - ext.len()].to_string();
        }
    }
    archive_name.to_string()
}

const SELECT_MOD: &str = "
    SELECT m.id, m.installation_id, m.name, m.version, m.archive_name, m.archive_sha256,
           m.staging_dir, m.installed_at,
           (SELECT COUNT(*) FROM mod_files f WHERE f.mod_id = m.id)
    FROM mods m";

fn row_to_mod(r: &rusqlite::Row<'_>) -> rusqlite::Result<Mod> {
    Ok(Mod {
        id: r.get(0)?,
        installation_id: r.get(1)?,
        name: r.get(2)?,
        version: r.get(3)?,
        archive_name: r.get(4)?,
        archive_sha256: r.get(5)?,
        staging_dir: r.get(6)?,
        installed_at: r.get(7)?,
        file_count: r.get(8)?,
    })
}

pub fn get(db: &Db, id: i64) -> Result<Mod> {
    db.conn
        .query_row(&format!("{SELECT_MOD} WHERE m.id = ?1"), [id], row_to_mod)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("mod {id}")))
}

fn find_by_name(db: &Db, inst_id: i64, name: &str) -> Result<Option<Mod>> {
    Ok(db
        .conn
        .query_row(
            &format!("{SELECT_MOD} WHERE m.installation_id = ?1 AND lower(m.name) = lower(?2)"),
            params![inst_id, name],
            row_to_mod,
        )
        .optional()?)
}

fn find_by_archive(db: &Db, inst_id: i64, sha256: &str) -> Result<Option<String>> {
    Ok(db
        .conn
        .query_row(
            "SELECT name FROM mods WHERE installation_id = ?1 AND archive_sha256 = ?2",
            params![inst_id, sha256],
            |r| r.get(0),
        )
        .optional()?)
}

/// Resolve a user-supplied mod selector: numeric id, exact name, or unique
/// case-insensitive name prefix.
pub fn find(db: &Db, inst_id: i64, selector: &str) -> Result<Mod> {
    if let Ok(id) = selector.parse::<i64>() {
        let m = get(db, id)?;
        if m.installation_id != inst_id {
            return Err(Error::NotFound(format!(
                "mod {id} belongs to a different installation"
            )));
        }
        return Ok(m);
    }
    if let Some(m) = find_by_name(db, inst_id, selector)? {
        return Ok(m);
    }
    let mut stmt = db.conn.prepare(&format!(
        "{SELECT_MOD} WHERE m.installation_id = ?1 AND lower(m.name) LIKE lower(?2) || '%'"
    ))?;
    let matches: Vec<Mod> = stmt
        .query_map(params![inst_id, selector], row_to_mod)?
        .collect::<rusqlite::Result<_>>()?;
    match matches.len() {
        0 => Err(Error::NotFound(format!("mod '{selector}'"))),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        _ => Err(Error::Ambiguous(format!(
            "'{selector}' matches multiple mods: {}",
            matches
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

pub fn files(db: &Db, mod_id: i64) -> Result<Vec<ModFile>> {
    let mut stmt = db.conn.prepare(
        "SELECT mod_id, rel_path, path_key, size, sha256 FROM mod_files
         WHERE mod_id = ?1 ORDER BY path_key",
    )?;
    let rows = stmt.query_map([mod_id], |r| {
        Ok(ModFile {
            mod_id: r.get(0)?,
            rel_path: r.get(1)?,
            path_key: r.get(2)?,
            size: r.get(3)?,
            sha256: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Uninstall a mod: refuse while any of its files are deployed (the game
/// directory would be left with files the database no longer explains).
pub fn uninstall(ctx: &Context, inst: &Installation, mod_id: i64) -> Result<()> {
    let m = get(&ctx.db, mod_id)?;
    if m.installation_id != inst.id {
        return Err(Error::NotFound(format!("mod {mod_id}")));
    }
    let deployed: i64 = ctx.db.conn.query_row(
        "SELECT COUNT(*) FROM deployed_files WHERE provider_mod_id = ?1",
        [mod_id],
        |r| r.get(0),
    )?;
    if deployed > 0 {
        return Err(Error::Blocked(format!(
            "'{}' has {deployed} deployed files; disable it and run 'deploy' (or 'purge') first",
            m.name
        )));
    }
    // DB first, then staging: if staging removal fails we have an orphan
    // directory (harmless, invisible), never a mod record without files.
    ctx.db
        .conn
        .execute("DELETE FROM mods WHERE id = ?1", [mod_id])?;
    staging::remove_staged(&ctx.paths, &m.staging_dir)?;
    Ok(())
}

/// Mods of the active profile in load order (lowest priority first).
pub fn list_for_profile(db: &Db, profile_id: i64) -> Result<Vec<crate::model::ProfileMod>> {
    let mut stmt = db.conn.prepare(&format!(
        "{SELECT_MOD} JOIN profile_mods pm ON pm.mod_id = m.id
         WHERE pm.profile_id = ?1 ORDER BY pm.priority"
    ))?;
    // Reuse the mod mapper, then fetch the pm columns with a second query to
    // keep the row shape single-sourced.
    let base: Vec<Mod> = stmt
        .query_map([profile_id], row_to_mod)?
        .collect::<rusqlite::Result<_>>()?;
    let mut result = Vec::with_capacity(base.len());
    for info in base {
        let (enabled, priority): (bool, i64) = db.conn.query_row(
            "SELECT enabled, priority FROM profile_mods WHERE profile_id = ?1 AND mod_id = ?2",
            params![profile_id, info.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        result.push(crate::model::ProfileMod {
            info,
            enabled,
            priority,
        });
    }
    Ok(result)
}

/// Enable or disable mods in a profile. Takes effect at the next deploy.
pub fn set_enabled(db: &Db, profile_id: i64, mod_ids: &[i64], enabled: bool) -> Result<()> {
    let tx = db.conn.unchecked_transaction()?;
    for &mod_id in mod_ids {
        let n = tx.execute(
            "UPDATE profile_mods SET enabled = ?1 WHERE profile_id = ?2 AND mod_id = ?3",
            params![enabled, profile_id, mod_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("mod {mod_id} in profile")));
        }
    }
    tx.commit()?;
    Ok(())
}

/// Move a mod to a 1-based position in the load order and renumber
/// contiguously. Position n = wins over positions < n.
pub fn set_position(db: &Db, profile_id: i64, mod_id: i64, position: i64) -> Result<()> {
    let mut order: Vec<i64> = {
        let mut stmt = db
            .conn
            .prepare("SELECT mod_id FROM profile_mods WHERE profile_id = ?1 ORDER BY priority")?;
        let rows = stmt.query_map([profile_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let from = order
        .iter()
        .position(|&id| id == mod_id)
        .ok_or_else(|| Error::NotFound(format!("mod {mod_id} in profile")))?;
    let to = (position - 1).clamp(0, order.len() as i64 - 1) as usize;
    let moved = order.remove(from);
    order.insert(to, moved);

    let tx = db.conn.unchecked_transaction()?;
    // Two-phase renumber to dodge the UNIQUE(profile_id, priority) constraint.
    tx.execute(
        "UPDATE profile_mods SET priority = -priority WHERE profile_id = ?1",
        [profile_id],
    )?;
    {
        let mut stmt = tx.prepare(
            "UPDATE profile_mods SET priority = ?1 WHERE profile_id = ?2 AND mod_id = ?3",
        )?;
        for (i, id) in order.iter().enumerate() {
            stmt.execute(params![(i + 1) as i64, profile_id, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Convenience: active profile id of an installation, which always exists.
pub fn active_profile_id(ctx: &Context, inst: &Installation) -> Result<i64> {
    match inst.active_profile_id {
        Some(id) => Ok(id),
        None => profile::repair_active_profile(&ctx.db, inst.id),
    }
}
