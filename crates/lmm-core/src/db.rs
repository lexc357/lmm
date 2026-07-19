use std::path::Path;

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Database handle. All lmm state lives in one SQLite file.
pub struct Db {
    pub conn: Connection,
}

/// Migrations, applied in order; `PRAGMA user_version` records progress.
/// Never edit an existing migration — append a new one.
const MIGRATIONS: &[&str] = &[
    // v1: full initial schema (see docs/DESIGN.md §3).
    "
    CREATE TABLE games (
        id           INTEGER PRIMARY KEY,
        slug         TEXT NOT NULL UNIQUE,
        name         TEXT NOT NULL,
        steam_app_id INTEGER
    );

    CREATE TABLE installations (
        id                INTEGER PRIMARY KEY,
        game_id           INTEGER NOT NULL REFERENCES games(id),
        path              TEXT NOT NULL UNIQUE,
        source            TEXT NOT NULL CHECK (source IN ('steam','manual')),
        steam_library     TEXT,
        proton_prefix     TEXT,
        label             TEXT,
        active_profile_id INTEGER REFERENCES profiles(id),
        created_at        INTEGER NOT NULL
    );

    CREATE TABLE profiles (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
        name            TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        UNIQUE (installation_id, name)
    );

    CREATE TABLE mods (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
        name            TEXT NOT NULL,
        version         TEXT,
        archive_name    TEXT NOT NULL,
        archive_sha256  TEXT NOT NULL,
        staging_dir     TEXT NOT NULL,
        nexus_mod_id    INTEGER,
        installed_at    INTEGER NOT NULL,
        UNIQUE (installation_id, name)
    );

    CREATE TABLE mod_files (
        id       INTEGER PRIMARY KEY,
        mod_id   INTEGER NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
        rel_path TEXT NOT NULL,
        path_key TEXT NOT NULL,
        size     INTEGER NOT NULL,
        sha256   TEXT NOT NULL,
        UNIQUE (mod_id, path_key)
    );
    CREATE INDEX idx_mod_files_path_key ON mod_files(path_key);

    CREATE TABLE profile_mods (
        profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
        mod_id     INTEGER NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
        enabled    INTEGER NOT NULL DEFAULT 0,
        priority   INTEGER NOT NULL,
        PRIMARY KEY (profile_id, mod_id),
        UNIQUE (profile_id, priority)
    );

    CREATE TABLE backups (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id),
        path_key        TEXT NOT NULL,
        rel_path        TEXT NOT NULL,
        backup_path     TEXT NOT NULL,
        sha256          TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        UNIQUE (installation_id, path_key)
    );

    CREATE TABLE deployed_files (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id),
        path_key        TEXT NOT NULL,
        rel_path        TEXT NOT NULL,
        provider_mod_id INTEGER NOT NULL REFERENCES mods(id),
        sha256          TEXT NOT NULL,
        backup_id       INTEGER REFERENCES backups(id),
        UNIQUE (installation_id, path_key)
    );

    CREATE TABLE deployments (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id),
        profile_id      INTEGER REFERENCES profiles(id),
        kind            TEXT NOT NULL CHECK (kind IN ('deploy','purge')),
        status          TEXT NOT NULL CHECK (status IN ('running','committed','rolled_back')),
        started_at      INTEGER NOT NULL,
        finished_at     INTEGER
    );

    CREATE TABLE journal (
        id            INTEGER PRIMARY KEY,
        deployment_id INTEGER NOT NULL REFERENCES deployments(id),
        seq           INTEGER NOT NULL,
        op            TEXT NOT NULL CHECK (op IN ('backup','write','remove','restore')),
        rel_path      TEXT NOT NULL,
        path_key      TEXT NOT NULL,
        mod_id        INTEGER,
        backup_id     INTEGER,
        pre_sha256    TEXT,
        new_sha256    TEXT,
        state         TEXT NOT NULL CHECK (state IN ('pending','done','undone')),
        UNIQUE (deployment_id, seq)
    );

    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    // v2: Nexus Mods download queue. Rows are the single source of truth for
    // download state; the shell, one-shot CLI and background workers all read
    // and write this table (SQLite WAL + busy_timeout make that safe).
    // Downloads are keyed by Nexus game domain, not installation id, so a mod
    // can be downloaded before its game is registered with lmm.
    "
    CREATE TABLE downloads (
        id            INTEGER PRIMARY KEY,
        game_domain   TEXT NOT NULL,      -- Nexus domain, e.g. 'skyrimspecialedition'
        nexus_mod_id  INTEGER NOT NULL,
        nexus_file_id INTEGER NOT NULL,
        nxm_key       TEXT,               -- short-lived download key from the nxm link
        nxm_expires   INTEGER,            -- unix expiry of that key (0 = unknown)
        mod_name      TEXT,               -- resolved from the Nexus API, may be NULL until resolved
        file_name     TEXT,               -- ditto
        version       TEXT,
        total_bytes   INTEGER,
        bytes_done    INTEGER NOT NULL DEFAULT 0,
        status        TEXT NOT NULL CHECK (status IN ('pending','active','completed','failed')),
        error         TEXT,
        archive_path  TEXT,               -- final path in the downloads dir once completed
        sha256        TEXT,               -- of the completed archive
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    );
    CREATE INDEX idx_downloads_status ON downloads(status);
    ",
    // v3: FOMOD installer records. One row per mod installed through the
    // interactive installer: the choices made (with generated flags), the
    // ModuleConfig hash they were made against, and the final file plan —
    // enough to show, replay, or reconfigure the installation. The archive
    // hash already lives on mods.archive_sha256.
    "
    CREATE TABLE fomod_installs (
        mod_id        INTEGER PRIMARY KEY REFERENCES mods(id) ON DELETE CASCADE,
        module_name   TEXT NOT NULL,
        config_sha256 TEXT NOT NULL,
        format        TEXT NOT NULL,
        choices_json  TEXT NOT NULL,
        plan_json     TEXT NOT NULL,
        created_at    INTEGER NOT NULL
    );
    ",
    // v4: Game Tools. Managed tool installs keep a full file manifest
    // (mirroring mods/mod_files) so tools can be verified, updated and
    // removed; config_backups records the pre-lmm content of every INI
    // file the game-configuration tweaks touch (NULL backup_path = the
    // file did not exist before lmm created it).
    "
    CREATE TABLE tools (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
        tool_id         TEXT NOT NULL,
        version         TEXT,
        archive_name    TEXT NOT NULL,
        archive_sha256  TEXT NOT NULL,
        installed_at    INTEGER NOT NULL,
        UNIQUE (installation_id, tool_id)
    );

    CREATE TABLE tool_files (
        id           INTEGER PRIMARY KEY,
        tool_row_id  INTEGER NOT NULL REFERENCES tools(id) ON DELETE CASCADE,
        rel_path     TEXT NOT NULL,
        path_key     TEXT NOT NULL,
        size         INTEGER NOT NULL,
        sha256       TEXT NOT NULL,
        backup_path  TEXT,
        UNIQUE (tool_row_id, path_key)
    );

    CREATE TABLE config_backups (
        id              INTEGER PRIMARY KEY,
        installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
        file            TEXT NOT NULL,
        backup_path     TEXT,
        sha256          TEXT,
        created_at      INTEGER NOT NULL,
        UNIQUE (installation_id, file)
    );
    ",
];

impl Db {
    /// Open (creating if needed) the database and bring the schema up to date.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)
            .map_err(|e| Error::Config(format!("cannot open database {}: {e}", path.display())))?;
        Self::init(conn)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        // WAL keeps readers unblocked; NORMAL sync is durable-enough for WAL
        // and much faster. Foreign keys are off by default in SQLite.
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(Error::Config(format!(
                "database schema v{version} is newer than this lmm supports (v{}); upgrade lmm",
                MIGRATIONS.len()
            )));
        }
        while (version as usize) < MIGRATIONS.len() {
            let sql = MIGRATIONS[version as usize];
            version += 1;
            // Each migration is atomic: schema change + version bump together.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(Db { conn })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

/// Current unix time in seconds; stored in every *_at column.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap(); // re-open: no re-apply
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
    }

    #[test]
    fn settings_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.setting("k").unwrap(), None);
        db.set_setting("k", "v1").unwrap();
        db.set_setting("k", "v2").unwrap();
        assert_eq!(db.setting("k").unwrap(), Some("v2".into()));
    }
}
