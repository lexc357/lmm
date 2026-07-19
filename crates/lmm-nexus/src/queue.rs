//! Persistent download queue, stored in lmm's SQLite database.
//!
//! Lifecycle of a row:
//!
//! ```text
//!             enqueue()                    mark_active()
//! (nxm link) ----------> pending ---------------------------> active
//!                          ^                                  |    |
//!                          | retry()            mark_failed() |    | mark_completed()
//!                          +-------- failed <-----------------+    +--> completed
//! ```
//!
//! Nothing moves forward automatically: a pending row only becomes active
//! when the user explicitly starts it (`downloads start`), which is where
//! "ask for confirmation before downloading" lives.
//!
//! The table is shared between the shell, its background download workers and
//! one-shot CLI invocations, each with their own SQLite connection; WAL mode
//! plus `busy_timeout` (set in lmm-core's `Db::open`) make concurrent access
//! safe. Cancellation also runs through the table: `request_cancel` flips an
//! active row back to failed, and the worker notices on its next progress
//! write (`set_progress` reports whether the row is still active).

use lmm_core::db::{Db, now};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::nxm::NxmLink;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Active,
    Completed,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Active => "active",
            Status::Completed => "completed",
            Status::Failed => "failed",
        }
    }

    fn from_str(s: &str) -> Status {
        match s {
            "pending" => Status::Pending,
            "active" => Status::Active,
            "completed" => Status::Completed,
            _ => Status::Failed,
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One download request. `mod_name`/`file_name`/... stay None until metadata
/// has been resolved via the Nexus API.
#[derive(Debug, Clone, Serialize)]
pub struct Download {
    pub id: i64,
    pub game_domain: String,
    pub nexus_mod_id: u64,
    pub nexus_file_id: u64,
    #[serde(skip)] // credential: never serialized into --json output
    pub nxm_key: Option<String>,
    #[serde(skip)]
    pub nxm_expires: Option<i64>,
    pub mod_name: Option<String>,
    pub file_name: Option<String>,
    pub version: Option<String>,
    pub total_bytes: Option<i64>,
    pub bytes_done: i64,
    pub status: Status,
    pub error: Option<String>,
    pub archive_path: Option<String>,
    pub sha256: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Download {
    /// Human-readable one-liner: resolved name if known, ids otherwise.
    pub fn describe(&self) -> String {
        match (&self.mod_name, &self.file_name) {
            (Some(m), Some(f)) => format!("'{m}' ({f})"),
            (Some(m), None) => format!("'{m}'"),
            _ => format!(
                "{}/mods/{}/files/{}",
                self.game_domain, self.nexus_mod_id, self.nexus_file_id
            ),
        }
    }

    /// The nxm credential for this row, if one was stored.
    pub fn link(&self) -> NxmLink {
        NxmLink {
            domain: self.game_domain.clone(),
            mod_id: self.nexus_mod_id,
            file_id: self.nexus_file_id,
            key: self.nxm_key.clone(),
            expires: self.nxm_expires,
        }
    }
}

const COLS: &str = "id, game_domain, nexus_mod_id, nexus_file_id, nxm_key, nxm_expires,
    mod_name, file_name, version, total_bytes, bytes_done, status, error,
    archive_path, sha256, created_at, updated_at";

fn row_to_download(r: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
    Ok(Download {
        id: r.get(0)?,
        game_domain: r.get(1)?,
        nexus_mod_id: r.get::<_, i64>(2)? as u64,
        nexus_file_id: r.get::<_, i64>(3)? as u64,
        nxm_key: r.get(4)?,
        nxm_expires: r.get(5)?,
        mod_name: r.get(6)?,
        file_name: r.get(7)?,
        version: r.get(8)?,
        total_bytes: r.get(9)?,
        bytes_done: r.get(10)?,
        status: Status::from_str(&r.get::<_, String>(11)?),
        error: r.get(12)?,
        archive_path: r.get(13)?,
        sha256: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

/// Add a validated link as a pending download.
///
/// If the same file is already pending or failed, the existing row is
/// refreshed with the new key (clicking the button again on Nexus is the
/// natural way to retry an expired link) instead of piling up duplicates.
/// An already active or completed download is left alone and returned as-is.
pub fn enqueue(db: &Db, link: &NxmLink) -> Result<(Download, bool)> {
    let existing = db
        .conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM downloads
                 WHERE game_domain = ?1 AND nexus_mod_id = ?2 AND nexus_file_id = ?3
                 ORDER BY id DESC LIMIT 1"
            ),
            params![link.domain, link.mod_id as i64, link.file_id as i64],
            row_to_download,
        )
        .optional()?;

    if let Some(d) = existing {
        match d.status {
            Status::Active | Status::Completed => return Ok((d, false)),
            Status::Pending | Status::Failed => {
                db.conn.execute(
                    "UPDATE downloads
                     SET nxm_key = ?1, nxm_expires = ?2, status = 'pending',
                         error = NULL, bytes_done = 0, updated_at = ?3
                     WHERE id = ?4",
                    params![link.key, link.expires, now(), d.id],
                )?;
                return Ok((get(db, d.id)?, true));
            }
        }
    }

    db.conn.execute(
        "INSERT INTO downloads
         (game_domain, nexus_mod_id, nexus_file_id, nxm_key, nxm_expires,
          status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
        params![
            link.domain,
            link.mod_id as i64,
            link.file_id as i64,
            link.key,
            link.expires,
            now(),
        ],
    )?;
    Ok((get(db, db.conn.last_insert_rowid())?, true))
}

pub fn get(db: &Db, id: i64) -> Result<Download> {
    db.conn
        .query_row(
            &format!("SELECT {COLS} FROM downloads WHERE id = ?1"),
            [id],
            row_to_download,
        )
        .optional()?
        .ok_or_else(|| Error::Other(format!("no download with id {id}")))
}

/// All downloads, newest first; optionally only one status.
pub fn list(db: &Db, status: Option<Status>) -> Result<Vec<Download>> {
    let (sql, param): (String, Vec<String>) = match status {
        Some(s) => (
            format!("SELECT {COLS} FROM downloads WHERE status = ?1 ORDER BY id DESC"),
            vec![s.as_str().to_string()],
        ),
        None => (
            format!("SELECT {COLS} FROM downloads ORDER BY id DESC"),
            vec![],
        ),
    };
    let mut stmt = db.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(param.iter()), row_to_download)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Store metadata resolved from the Nexus API.
pub fn set_resolved(
    db: &Db,
    id: i64,
    mod_name: Option<&str>,
    file_name: Option<&str>,
    version: Option<&str>,
    total_bytes: Option<i64>,
) -> Result<()> {
    db.conn.execute(
        "UPDATE downloads SET mod_name = ?1, file_name = ?2, version = ?3,
             total_bytes = ?4, updated_at = ?5 WHERE id = ?6",
        params![mod_name, file_name, version, total_bytes, now(), id],
    )?;
    Ok(())
}

/// pending/failed -> active. Fails if the row is in any other state so two
/// workers can never both claim it (the UPDATE is atomic).
pub fn mark_active(db: &Db, id: i64) -> Result<()> {
    let n = db.conn.execute(
        "UPDATE downloads SET status = 'active', error = NULL, bytes_done = 0,
             updated_at = ?1 WHERE id = ?2 AND status IN ('pending','failed')",
        params![now(), id],
    )?;
    if n == 0 {
        let d = get(db, id)?;
        return Err(Error::Other(format!(
            "download {id} is {}, not pending",
            d.status
        )));
    }
    Ok(())
}

/// Progress heartbeat from a worker. Returns false if the row is no longer
/// active — that is the cancellation signal, and the worker must abort.
pub fn set_progress(db: &Db, id: i64, bytes_done: i64) -> Result<bool> {
    let n = db.conn.execute(
        "UPDATE downloads SET bytes_done = ?1, updated_at = ?2
         WHERE id = ?3 AND status = 'active'",
        params![bytes_done, now(), id],
    )?;
    Ok(n == 1)
}

/// active -> completed. Clears the stored nxm key: it is spent and there is
/// no reason to keep a credential in the database longer than needed.
pub fn mark_completed(db: &Db, id: i64, archive_path: &str, sha256: &str, size: i64) -> Result<()> {
    db.conn.execute(
        "UPDATE downloads SET status = 'completed', archive_path = ?1, sha256 = ?2,
             bytes_done = ?3, total_bytes = ?3, nxm_key = NULL, nxm_expires = NULL,
             error = NULL, updated_at = ?4
         WHERE id = ?5",
        params![archive_path, sha256, size, now(), id],
    )?;
    Ok(())
}

pub fn mark_failed(db: &Db, id: i64, error: &str) -> Result<()> {
    db.conn.execute(
        "UPDATE downloads SET status = 'failed', error = ?1, updated_at = ?2 WHERE id = ?3",
        params![error, now(), id],
    )?;
    Ok(())
}

/// Ask a running worker to stop (active -> failed). The worker sees the state
/// change on its next `set_progress` and abandons the transfer. Also works on
/// pending rows (they simply never start).
pub fn request_cancel(db: &Db, id: i64) -> Result<()> {
    let n = db.conn.execute(
        "UPDATE downloads SET status = 'failed', error = 'cancelled', updated_at = ?1
         WHERE id = ?2 AND status IN ('pending','active')",
        params![now(), id],
    )?;
    if n == 0 {
        let d = get(db, id)?;
        return Err(Error::Other(format!(
            "download {id} is {}; only pending or active downloads can be cancelled",
            d.status
        )));
    }
    Ok(())
}

/// Delete a finished (completed/failed) row. The archive file, if any, is
/// left on disk — deleting user data is not this function's call.
pub fn remove(db: &Db, id: i64) -> Result<()> {
    let d = get(db, id)?;
    if matches!(d.status, Status::Pending | Status::Active) {
        return Err(Error::Other(format!(
            "download {id} is {}; cancel it first",
            d.status
        )));
    }
    db.conn
        .execute("DELETE FROM downloads WHERE id = ?1", [id])?;
    Ok(())
}

/// Rows that crashed mid-transfer in a previous run (status 'active' at
/// startup can only be leftovers — workers die with the process).
pub fn reset_stale_active(db: &Db) -> Result<usize> {
    Ok(db.conn.execute(
        "UPDATE downloads SET status = 'failed', error = 'interrupted (lmm exited)',
             updated_at = ?1 WHERE status = 'active'",
        params![now()],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        Db::open_in_memory().expect("in-memory db")
    }

    fn link(file_id: u64) -> NxmLink {
        NxmLink {
            domain: "skyrimspecialedition".into(),
            mod_id: 12604,
            file_id,
            key: Some("k1".into()),
            expires: Some(9_999_999_999),
        }
    }

    #[test]
    fn enqueue_dedupes_and_refreshes_key() {
        let db = test_db();
        let (d1, fresh1) = enqueue(&db, &link(1)).unwrap();
        assert!(fresh1);
        assert_eq!(d1.status, Status::Pending);

        let mut l2 = link(1);
        l2.key = Some("k2".into());
        let (d2, fresh2) = enqueue(&db, &l2).unwrap();
        assert!(fresh2);
        assert_eq!(d1.id, d2.id, "same file reuses the row");
        assert_eq!(d2.nxm_key.as_deref(), Some("k2"));

        let (other, _) = enqueue(&db, &link(2)).unwrap();
        assert_ne!(other.id, d1.id);
        assert_eq!(list(&db, None).unwrap().len(), 2);
    }

    #[test]
    fn full_lifecycle() {
        let db = test_db();
        let (d, _) = enqueue(&db, &link(1)).unwrap();
        set_resolved(
            &db,
            d.id,
            Some("SkyUI"),
            Some("SkyUI_5_2.7z"),
            Some("5.2"),
            Some(1000),
        )
        .unwrap();
        mark_active(&db, d.id).unwrap();
        assert!(mark_active(&db, d.id).is_err(), "already active");
        assert!(set_progress(&db, d.id, 512).unwrap());
        mark_completed(&db, d.id, "/tmp/SkyUI_5_2.7z", "abc123", 1000).unwrap();

        let d = get(&db, d.id).unwrap();
        assert_eq!(d.status, Status::Completed);
        assert_eq!(d.nxm_key, None, "key cleared after completion");
        assert_eq!(d.describe(), "'SkyUI' (SkyUI_5_2.7z)");
        assert!(remove(&db, d.id).is_ok());
    }

    #[test]
    fn cancel_flips_active_row_and_worker_notices() {
        let db = test_db();
        let (d, _) = enqueue(&db, &link(1)).unwrap();
        mark_active(&db, d.id).unwrap();
        request_cancel(&db, d.id).unwrap();
        assert!(!set_progress(&db, d.id, 10).unwrap(), "worker told to stop");
        assert_eq!(get(&db, d.id).unwrap().status, Status::Failed);
        assert!(request_cancel(&db, d.id).is_err(), "already finished");
    }

    #[test]
    fn failed_can_retry_via_enqueue() {
        let db = test_db();
        let (d, _) = enqueue(&db, &link(1)).unwrap();
        mark_active(&db, d.id).unwrap();
        mark_failed(&db, d.id, "network").unwrap();
        let (d2, fresh) = enqueue(&db, &link(1)).unwrap();
        assert!(fresh);
        assert_eq!(d2.id, d.id);
        assert_eq!(d2.status, Status::Pending);
        assert_eq!(d2.error, None);
    }

    #[test]
    fn stale_active_rows_reset_on_startup() {
        let db = test_db();
        let (d, _) = enqueue(&db, &link(1)).unwrap();
        mark_active(&db, d.id).unwrap();
        assert_eq!(reset_stale_active(&db).unwrap(), 1);
        assert_eq!(get(&db, d.id).unwrap().status, Status::Failed);
    }
}
