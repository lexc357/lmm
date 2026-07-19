//! Desired-state resolution and conflict reporting.
//!
//! The desired state of a profile is: for every path provided by at least
//! one enabled mod, the file from the highest-priority provider (last in the
//! load order wins). This is pure database computation — deployment diffs it
//! against reality.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::db::Db;
use crate::error::Result;

/// One mod's claim on a path.
#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub mod_id: i64,
    pub mod_name: String,
    pub priority: i64,
    /// Path casing as this mod ships it.
    pub rel_path: String,
    pub size: i64,
    pub sha256: String,
}

/// All enabled providers for every path, keyed by case-insensitive path key,
/// providers sorted by descending priority (winner first).
pub fn provider_map(db: &Db, profile_id: i64) -> Result<BTreeMap<String, Vec<Provider>>> {
    let mut stmt = db.conn.prepare(
        "SELECT f.path_key, f.rel_path, f.size, f.sha256, m.id, m.name, pm.priority
         FROM profile_mods pm
         JOIN mods m      ON m.id = pm.mod_id
         JOIN mod_files f ON f.mod_id = m.id
         WHERE pm.profile_id = ?1 AND pm.enabled = 1
         ORDER BY f.path_key, pm.priority DESC",
    )?;
    let rows = stmt.query_map([profile_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Provider {
                rel_path: r.get(1)?,
                size: r.get(2)?,
                sha256: r.get(3)?,
                mod_id: r.get(4)?,
                mod_name: r.get(5)?,
                priority: r.get(6)?,
            },
        ))
    })?;

    let mut map: BTreeMap<String, Vec<Provider>> = BTreeMap::new();
    for row in rows {
        let (key, provider) = row?;
        map.entry(key).or_default().push(provider);
    }
    Ok(map)
}

/// Desired state: winning provider per path key.
pub fn desired_state(db: &Db, profile_id: i64) -> Result<BTreeMap<String, Provider>> {
    Ok(provider_map(db, profile_id)?
        .into_iter()
        .map(|(key, mut providers)| (key, providers.swap_remove(0)))
        .collect())
}

/// A path claimed by more than one enabled mod.
#[derive(Debug, Serialize)]
pub struct Conflict {
    pub path_key: String,
    /// Winner first (highest priority).
    pub providers: Vec<Provider>,
}

pub fn conflicts(db: &Db, profile_id: i64) -> Result<Vec<Conflict>> {
    Ok(provider_map(db, profile_id)?
        .into_iter()
        .filter(|(_, providers)| providers.len() > 1)
        .map(|(path_key, providers)| Conflict {
            path_key,
            providers,
        })
        .collect())
}
