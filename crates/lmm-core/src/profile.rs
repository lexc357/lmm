//! Profiles: named mod configurations (enabled state + load order) per
//! installation. Exactly one profile is active per installation; switching
//! profiles changes desired state only — `deploy` applies it.

use rusqlite::{OptionalExtension, params};

use crate::db::{Db, now};
use crate::error::{Error, Result};
use crate::model::Profile;

pub fn list(db: &Db, inst_id: i64) -> Result<Vec<Profile>> {
    let active = active_id(db, inst_id)?;
    let mut stmt = db.conn.prepare(
        "SELECT id, installation_id, name, created_at FROM profiles
         WHERE installation_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([inst_id], |r| {
        Ok(Profile {
            id: r.get(0)?,
            installation_id: r.get(1)?,
            name: r.get(2)?,
            is_active: false,
            created_at: r.get(3)?,
        })
    })?;
    let mut profiles: Vec<Profile> = rows.collect::<rusqlite::Result<_>>()?;
    for p in &mut profiles {
        p.is_active = Some(p.id) == active;
    }
    Ok(profiles)
}

pub fn find(db: &Db, inst_id: i64, name: &str) -> Result<Profile> {
    let active = active_id(db, inst_id)?;
    db.conn
        .query_row(
            "SELECT id, installation_id, name, created_at FROM profiles
             WHERE installation_id = ?1 AND lower(name) = lower(?2)",
            params![inst_id, name],
            |r| {
                Ok(Profile {
                    id: r.get(0)?,
                    installation_id: r.get(1)?,
                    name: r.get(2)?,
                    is_active: false,
                    created_at: r.get(3)?,
                })
            },
        )
        .optional()?
        .map(|mut p| {
            p.is_active = Some(p.id) == active;
            p
        })
        .ok_or_else(|| Error::NotFound(format!("profile '{name}'")))
}

fn active_id(db: &Db, inst_id: i64) -> Result<Option<i64>> {
    Ok(db.conn.query_row(
        "SELECT active_profile_id FROM installations WHERE id = ?1",
        [inst_id],
        |r| r.get(0),
    )?)
}

/// Create a profile with every installed mod present but disabled, in the
/// same order as the currently active profile (a fresh start that still has
/// a sensible load order).
pub fn create(db: &Db, inst_id: i64, name: &str) -> Result<Profile> {
    validate_name(name)?;
    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO profiles (installation_id, name, created_at) VALUES (?1, ?2, ?3)",
        params![inst_id, name, now()],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::Invalid(format!("profile '{name}' already exists"))
        }
        e => e.into(),
    })?;
    let new_id = tx.last_insert_rowid();

    // Order template: the active profile if it has entries, else mod id.
    let template = active_id(db, inst_id)?;
    tx.execute(
        "INSERT INTO profile_mods (profile_id, mod_id, enabled, priority)
         SELECT ?1, m.id, 0,
                ROW_NUMBER() OVER (ORDER BY COALESCE(pm.priority, m.id))
         FROM mods m
         LEFT JOIN profile_mods pm ON pm.mod_id = m.id AND pm.profile_id = ?3
         WHERE m.installation_id = ?2",
        params![new_id, inst_id, template],
    )?;
    tx.commit()?;
    find(db, inst_id, name)
}

/// Duplicate a profile including enabled state and load order.
pub fn copy(db: &Db, inst_id: i64, from: &str, to: &str) -> Result<Profile> {
    validate_name(to)?;
    let src = find(db, inst_id, from)?;
    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO profiles (installation_id, name, created_at) VALUES (?1, ?2, ?3)",
        params![inst_id, to, now()],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::Invalid(format!("profile '{to}' already exists"))
        }
        e => e.into(),
    })?;
    let new_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO profile_mods (profile_id, mod_id, enabled, priority)
         SELECT ?1, mod_id, enabled, priority FROM profile_mods WHERE profile_id = ?2",
        params![new_id, src.id],
    )?;
    tx.commit()?;
    find(db, inst_id, to)
}

/// Switching the active profile only changes what `deploy` will do next; the
/// game directory is untouched until then.
pub fn switch(db: &Db, inst_id: i64, name: &str) -> Result<Profile> {
    let p = find(db, inst_id, name)?;
    db.conn.execute(
        "UPDATE installations SET active_profile_id = ?1 WHERE id = ?2",
        params![p.id, inst_id],
    )?;
    find(db, inst_id, name)
}

pub fn delete(db: &Db, inst_id: i64, name: &str) -> Result<()> {
    let p = find(db, inst_id, name)?;
    if p.is_active {
        return Err(Error::Blocked(format!(
            "'{name}' is the active profile; switch to another profile first"
        )));
    }
    db.conn
        .execute("DELETE FROM profiles WHERE id = ?1", [p.id])?;
    Ok(())
}

/// Installations should always have an active profile; if the pointer was
/// lost (e.g. manual DB edits), restore or recreate a default.
pub fn repair_active_profile(db: &Db, inst_id: i64) -> Result<i64> {
    if let Some(id) = active_id(db, inst_id)? {
        return Ok(id);
    }
    let existing: Option<i64> = db
        .conn
        .query_row(
            "SELECT id FROM profiles WHERE installation_id = ?1 ORDER BY id LIMIT 1",
            [inst_id],
            |r| r.get(0),
        )
        .optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            db.conn.execute(
                "INSERT INTO profiles (installation_id, name, created_at)
                 VALUES (?1, 'default', ?2)",
                params![inst_id, now()],
            )?;
            db.conn.last_insert_rowid()
        }
    };
    db.conn.execute(
        "UPDATE installations SET active_profile_id = ?1 WHERE id = ?2",
        params![id, inst_id],
    )?;
    Ok(id)
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 64 {
        return Err(Error::Invalid(
            "profile name must be 1-64 characters".into(),
        ));
    }
    Ok(())
}
