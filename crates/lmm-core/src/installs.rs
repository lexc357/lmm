//! Registered game installations and selection of the "current" one.

use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::db::{Db, now};
use crate::error::{Error, Result};
use crate::games;
use crate::model::Installation;

const SELECT: &str = "
    SELECT i.id, g.slug, g.name, i.path, i.source, i.proton_prefix, i.label,
           i.active_profile_id, i.created_at
    FROM installations i JOIN games g ON g.id = i.game_id";

fn row_to_installation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Installation> {
    Ok(Installation {
        id: r.get(0)?,
        game_slug: r.get(1)?,
        game_name: r.get(2)?,
        path: r.get::<_, String>(3)?.into(),
        source: r.get(4)?,
        proton_prefix: r.get::<_, Option<String>>(5)?.map(Into::into),
        label: r.get(6)?,
        active_profile_id: r.get(7)?,
        created_at: r.get(8)?,
    })
}

pub struct NewInstallation<'a> {
    pub game_slug: &'a str,
    pub path: &'a Path,
    pub source: &'a str, // 'steam' | 'manual'
    pub steam_library: Option<&'a Path>,
    pub proton_prefix: Option<&'a Path>,
    pub label: Option<&'a str>,
}

/// Register an installation and create its initial "default" profile.
pub fn add(db: &Db, new: &NewInstallation) -> Result<Installation> {
    let game = games::by_slug(new.game_slug)
        .ok_or_else(|| Error::Invalid(format!("unknown game slug '{}'", new.game_slug)))?;

    // The path must exist and be a directory: everything downstream
    // (deployment, verification) assumes a real game root.
    let canon = new
        .path
        .canonicalize()
        .map_err(|e| Error::io(new.path, e))?;
    if !canon.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is not a directory",
            canon.display()
        )));
    }

    let game_id = games::game_id(db, game.slug)?;
    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO installations (game_id, path, source, steam_library, proton_prefix, label, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            game_id,
            canon.to_string_lossy(),
            new.source,
            new.steam_library.map(|p| p.to_string_lossy().into_owned()),
            new.proton_prefix.map(|p| p.to_string_lossy().into_owned()),
            new.label,
            now(),
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::Invalid(format!("{} is already registered", canon.display()))
        }
        e => e.into(),
    })?;
    let inst_id = tx.last_insert_rowid();

    // Every installation starts with an active "default" profile so that mod
    // commands work immediately.
    tx.execute(
        "INSERT INTO profiles (installation_id, name, created_at) VALUES (?1, 'default', ?2)",
        params![inst_id, now()],
    )?;
    let profile_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE installations SET active_profile_id = ?1 WHERE id = ?2",
        params![profile_id, inst_id],
    )?;
    tx.commit()?;

    get(db, inst_id)
}

pub fn get(db: &Db, id: i64) -> Result<Installation> {
    db.conn
        .query_row(
            &format!("{SELECT} WHERE i.id = ?1"),
            [id],
            row_to_installation,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("installation {id}")))
}

pub fn list(db: &Db) -> Result<Vec<Installation>> {
    let mut stmt = db.conn.prepare(&format!("{SELECT} ORDER BY i.id"))?;
    let rows = stmt.query_map([], row_to_installation)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Resolve which installation a command targets:
/// explicit selector > default_installation setting > the only one registered.
pub fn select(db: &Db, selector: Option<&str>) -> Result<Installation> {
    if let Some(sel) = selector {
        return find(db, sel);
    }
    if let Some(default) = db.setting("default_installation")? {
        let id: i64 = default
            .parse()
            .map_err(|_| Error::Invalid(format!("bad default_installation '{default}'")))?;
        return get(db, id);
    }
    let all = list(db)?;
    match all.len() {
        0 => Err(Error::Invalid(
            "no game installations registered; run 'scan' then 'game add'".into(),
        )),
        1 => Ok(all.into_iter().next().expect("len checked")),
        n => Err(Error::Ambiguous(format!(
            "{n} installations registered; pass --game <id|slug> or set a default with 'game use'"
        ))),
    }
}

/// Find by numeric id, game slug, or label (slug/label must be unambiguous).
pub fn find(db: &Db, selector: &str) -> Result<Installation> {
    if let Ok(id) = selector.parse::<i64>() {
        return get(db, id);
    }
    let all = list(db)?;
    let sel = selector.to_lowercase();
    let matches: Vec<_> = all
        .into_iter()
        .filter(|i| {
            i.game_slug.to_lowercase() == sel
                || i.label.as_deref().is_some_and(|l| l.to_lowercase() == sel)
        })
        .collect();
    match matches.len() {
        0 => Err(Error::NotFound(format!("installation '{selector}'"))),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        n => Err(Error::Ambiguous(format!(
            "'{selector}' matches {n} installations; use a numeric id"
        ))),
    }
}

pub fn set_default(db: &Db, id: i64) -> Result<()> {
    get(db, id)?; // must exist
    db.set_setting("default_installation", &id.to_string())
}

/// Unregister an installation. Refused while files are deployed: removing the
/// record would orphan managed files in the game directory.
pub fn remove(db: &Db, id: i64) -> Result<()> {
    let deployed: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM deployed_files WHERE installation_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if deployed > 0 {
        return Err(Error::Blocked(format!(
            "{deployed} files are deployed for this installation; run 'purge' first"
        )));
    }
    // Same reasoning for managed tools: dropping the record would orphan
    // tool files (and their displaced-original backups) in the game tree.
    let tools: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM tools WHERE installation_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if tools > 0 {
        return Err(Error::Blocked(format!(
            "{tools} tool(s) are installed for this installation; run 'tools remove' first"
        )));
    }
    // Clear the self-referencing FK before cascading profile deletion.
    db.conn.execute(
        "UPDATE installations SET active_profile_id = NULL WHERE id = ?1",
        [id],
    )?;
    db.conn
        .execute("DELETE FROM backups WHERE installation_id = ?1", [id])?;
    let n = db
        .conn
        .execute("DELETE FROM installations WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(Error::NotFound(format!("installation {id}")));
    }
    if db.setting("default_installation")? == Some(id.to_string()) {
        db.conn.execute(
            "DELETE FROM settings WHERE key = 'default_installation'",
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::sync_registry;

    fn setup() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        sync_registry(&db).unwrap();
        (db, dir)
    }

    fn add_generic(db: &Db, path: &Path) -> Installation {
        add(
            db,
            &NewInstallation {
                game_slug: "generic",
                path,
                source: "manual",
                steam_library: None,
                proton_prefix: None,
                label: Some("testgame"),
            },
        )
        .unwrap()
    }

    #[test]
    fn add_creates_default_profile_and_rejects_duplicates() {
        let (db, dir) = setup();
        let inst = add_generic(&db, dir.path());
        assert!(inst.active_profile_id.is_some());
        let err = add(
            &db,
            &NewInstallation {
                game_slug: "generic",
                path: dir.path(),
                source: "manual",
                steam_library: None,
                proton_prefix: None,
                label: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("already registered"), "{err}");
    }

    #[test]
    fn select_uses_only_installation_then_default() {
        let (db, dir) = setup();
        assert!(select(&db, None).is_err());
        let inst = add_generic(&db, dir.path());
        assert_eq!(select(&db, None).unwrap().id, inst.id);
        assert_eq!(select(&db, Some("testgame")).unwrap().id, inst.id);

        let dir2 = tempfile::tempdir().unwrap();
        add(
            &db,
            &NewInstallation {
                game_slug: "skyrimse",
                path: dir2.path(),
                source: "manual",
                steam_library: None,
                proton_prefix: None,
                label: None,
            },
        )
        .unwrap();
        assert!(matches!(select(&db, None), Err(Error::Ambiguous(_))));
        set_default(&db, inst.id).unwrap();
        assert_eq!(select(&db, None).unwrap().id, inst.id);
        assert_eq!(select(&db, Some("skyrimse")).unwrap().game_slug, "skyrimse");
    }

    #[test]
    fn remove_clears_default() {
        let (db, dir) = setup();
        let inst = add_generic(&db, dir.path());
        set_default(&db, inst.id).unwrap();
        remove(&db, inst.id).unwrap();
        assert_eq!(db.setting("default_installation").unwrap(), None);
        assert!(list(&db).unwrap().is_empty());
    }
}
