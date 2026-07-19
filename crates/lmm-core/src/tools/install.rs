//! Managed tool installs: extract a tool archive, place its files at the
//! tool's target root, and record a full manifest so the install can be
//! verified, updated and removed later.
//!
//! Tools deliberately do not go through the mod deployment pipeline: they
//! are not part of any profile, several target the game *root* rather than
//! the mod root, and standalone tools live outside the game entirely. The
//! same safety rules apply though: originals displaced by a tool file are
//! backed up first, nothing whose content lmm cannot account for is ever
//! overwritten or deleted without `force`, and a failed install undoes its
//! completed steps.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Context;
use crate::db::{Db, now};
use crate::error::{Error, IoContext, Result};
use crate::hash::sha256_file;
use crate::model::Installation;
use crate::paths::RelPath;
use crate::staging;
use crate::tools::registry::{Target, ToolDef};
use crate::tools::target_root;

/// The `tools` table row for a managed install.
#[derive(Debug, Clone, Serialize)]
pub struct ToolRecord {
    pub row_id: i64,
    pub tool_id: String,
    pub version: Option<String>,
    pub archive_name: String,
    pub archive_sha256: String,
    pub installed_at: i64,
}

/// One file of a managed install (rel to the tool's target root).
#[derive(Debug, Clone)]
pub struct ToolFile {
    pub rel: RelPath,
    pub size: i64,
    pub sha256: String,
    /// Backup of the unmanaged file this one displaced, rel to backups dir.
    pub backup_path: Option<String>,
}

pub fn get_record(db: &Db, inst_id: i64, tool_id: &str) -> Result<Option<ToolRecord>> {
    Ok(db
        .conn
        .query_row(
            "SELECT id, tool_id, version, archive_name, archive_sha256, installed_at
             FROM tools WHERE installation_id = ?1 AND tool_id = ?2",
            params![inst_id, tool_id],
            |r| {
                Ok(ToolRecord {
                    row_id: r.get(0)?,
                    tool_id: r.get(1)?,
                    version: r.get(2)?,
                    archive_name: r.get(3)?,
                    archive_sha256: r.get(4)?,
                    installed_at: r.get(5)?,
                })
            },
        )
        .optional()?)
}

pub fn files(db: &Db, tool_row_id: i64) -> Result<Vec<ToolFile>> {
    let mut stmt = db.conn.prepare(
        "SELECT rel_path, size, sha256, backup_path FROM tool_files
         WHERE tool_row_id = ?1 ORDER BY path_key",
    )?;
    let rows = stmt.query_map([tool_row_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (rel, size, sha256, backup_path) = row?;
        out.push(ToolFile {
            rel: RelPath::parse(&rel)?,
            size,
            sha256,
            backup_path,
        });
    }
    Ok(out)
}

/// Outcome of an install/update, for frontend display.
#[derive(Debug, Serialize)]
pub struct InstalledTool {
    pub tool_id: String,
    pub version: Option<String>,
    pub files: usize,
    /// Unmanaged files that were displaced and backed up.
    pub backed_up: usize,
    /// Files left over from the previous version that were removed.
    pub stale_removed: usize,
    pub target_root: PathBuf,
}

/// Install or update a tool from a local archive. Existing managed files are
/// replaced (their content is verified first — drift needs `force`); files
/// from the previous version that the new one no longer ships are removed;
/// unmanaged files in the way are backed up before being overwritten.
pub fn install(
    ctx: &Context,
    inst: &Installation,
    tool: &ToolDef,
    archive_path: &Path,
    version: Option<&str>,
    force: bool,
) -> Result<InstalledTool> {
    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, archive_path)?;
    let content_root = unwrap_wrappers(extracted.root().to_path_buf(), tool)?;
    let new_files = staging::inventory(&content_root)?;
    if new_files.is_empty() {
        return Err(Error::Archive("archive contains no files".into()));
    }

    let archive_name = archive_path
        .file_name()
        .ok_or_else(|| Error::Invalid(format!("{}: not a file", archive_path.display())))?
        .to_string_lossy()
        .into_owned();
    let version = version
        .map(str::to_string)
        .or_else(|| guess_version(&archive_name));

    let root = target_root(&ctx.paths, inst, tool);
    fs::create_dir_all(&root).path_ctx(&root)?;

    let old = get_record(&ctx.db, inst.id, tool.id)?;
    let old_files = match &old {
        Some(rec) => files(&ctx.db, rec.row_id)?,
        None => Vec::new(),
    };
    let old_by_key: std::collections::HashMap<String, &ToolFile> =
        old_files.iter().map(|f| (f.rel.key(), f)).collect();

    // ---- Preflight: every problem is found before anything is written. ----
    let mut backups_needed: Vec<(usize, PathBuf)> = Vec::new(); // (new_files idx, target)
    for (idx, f) in new_files.iter().enumerate() {
        let target = f.rel.to_native(&root);

        // A path already deployed by a mod belongs to the mod pipeline;
        // silently overwriting it would corrupt deploy's bookkeeping.
        if tool.target != Target::Standalone && deployed_owner(&ctx.db, inst.id, &f.rel.key())? {
            return Err(Error::Blocked(format!(
                "{} is currently deployed by a mod; this tool overlaps installed mods — \
                 install it as a mod instead ('install')",
                f.rel
            )));
        }

        if !target.exists() {
            continue;
        }
        let on_disk = sha256_file(&target)?;
        if let Some(prev) = old_by_key.get(&f.rel.key()) {
            if on_disk != prev.sha256 && on_disk != f.sha256 && !force {
                return Err(Error::Blocked(format!(
                    "{} was modified outside lmm since the tool was installed; \
                     rerun with --force to overwrite",
                    target.display()
                )));
            }
        } else if on_disk != f.sha256 {
            // Unmanaged file in the way: displace it into a backup.
            backups_needed.push((idx, target));
        }
    }

    // ---- Execute with best-effort undo on failure. ----
    let mut undo_written: Vec<PathBuf> = Vec::new();
    let mut undo_backups: Vec<(PathBuf, PathBuf)> = Vec::new(); // (backup, original)
    let mut backup_by_idx: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    let run = |undo_written: &mut Vec<PathBuf>,
               undo_backups: &mut Vec<(PathBuf, PathBuf)>,
               backup_by_idx: &mut std::collections::HashMap<usize, String>|
     -> Result<()> {
        let backup_dir_rel = format!("tools/{}", inst.id);
        for (seq, (idx, target)) in backups_needed.iter().enumerate() {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = format!("{backup_dir_rel}/{}-{seq}-{name}", now());
            let backup_abs = ctx.paths.backups_dir.join(&rel);
            if let Some(parent) = backup_abs.parent() {
                fs::create_dir_all(parent).path_ctx(parent)?;
            }
            // Move, not copy: the write below can never race a half-copy.
            fs::rename(target, &backup_abs).path_ctx(target)?;
            undo_backups.push((backup_abs, target.clone()));
            backup_by_idx.insert(*idx, rel);
        }

        for f in &new_files {
            let src = f.rel.to_native(&content_root);
            let target = f.rel.to_native(&root);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).path_ctx(parent)?;
            }
            let tmp = target.with_extension("lmm-tmp");
            fs::copy(&src, &tmp).path_ctx(&src)?;
            fs::rename(&tmp, &target).path_ctx(&target)?;
            undo_written.push(target);
        }
        Ok(())
    };

    if let Err(e) = run(&mut undo_written, &mut undo_backups, &mut backup_by_idx) {
        for path in undo_written.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for (backup, original) in undo_backups.iter().rev() {
            let _ = fs::rename(backup, original);
        }
        return Err(e);
    }

    // ---- Remove files the previous version shipped but the new one doesn't.
    let new_keys: std::collections::HashSet<String> =
        new_files.iter().map(|f| f.rel.key()).collect();
    let mut stale_removed = 0usize;
    let mut carried_backups: Vec<(String, String)> = Vec::new(); // (path_key, backup rel)
    for f in &old_files {
        if new_keys.contains(&f.rel.key()) {
            // Same path in the new version: carry a recorded backup forward
            // so the original still comes back when the tool is removed.
            if let Some(b) = &f.backup_path {
                carried_backups.push((f.rel.key(), b.clone()));
            }
            continue;
        }
        let target = f.rel.to_native(&root);
        match sha256_file(&target) {
            Ok(h) if h == f.sha256 || force => {
                fs::remove_file(&target).path_ctx(&target)?;
                stale_removed += 1;
                if let Some(b) = &f.backup_path {
                    let backup_abs = ctx.paths.backups_dir.join(b);
                    if backup_abs.exists() {
                        fs::rename(&backup_abs, &target).path_ctx(&backup_abs)?;
                    }
                }
            }
            // Missing or drifted: leave it alone, it is not ours to delete.
            _ => {}
        }
    }

    // ---- Record everything in one transaction. ----
    let tx = ctx.db.conn.unchecked_transaction()?;
    let row_id = match &old {
        Some(rec) => {
            tx.execute(
                "UPDATE tools SET version = ?1, archive_name = ?2, archive_sha256 = ?3,
                                  installed_at = ?4 WHERE id = ?5",
                params![
                    version,
                    archive_name,
                    extracted.archive_sha256,
                    now(),
                    rec.row_id
                ],
            )?;
            tx.execute(
                "DELETE FROM tool_files WHERE tool_row_id = ?1",
                [rec.row_id],
            )?;
            rec.row_id
        }
        None => {
            tx.execute(
                "INSERT INTO tools (installation_id, tool_id, version, archive_name,
                                    archive_sha256, installed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    inst.id,
                    tool.id,
                    version,
                    archive_name,
                    extracted.archive_sha256,
                    now()
                ],
            )?;
            tx.last_insert_rowid()
        }
    };
    {
        let carried: std::collections::HashMap<&str, &str> = carried_backups
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut stmt = tx.prepare(
            "INSERT INTO tool_files (tool_row_id, rel_path, path_key, size, sha256, backup_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (idx, f) in new_files.iter().enumerate() {
            let key = f.rel.key();
            let backup = backup_by_idx
                .get(&idx)
                .map(String::as_str)
                .or_else(|| carried.get(key.as_str()).copied());
            stmt.execute(params![
                row_id,
                f.rel.as_str(),
                key,
                f.size as i64,
                f.sha256,
                backup
            ])?;
        }
    }
    tx.commit()?;

    Ok(InstalledTool {
        tool_id: tool.id.to_string(),
        version,
        files: new_files.len(),
        backed_up: backup_by_idx.len(),
        stale_removed,
        target_root: root,
    })
}

/// Per-file result of `tools verify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileState {
    Ok,
    Missing,
    Modified,
}

#[derive(Debug, Serialize)]
pub struct VerifiedFile {
    pub rel_path: String,
    pub state: FileState,
}

/// Re-hash every recorded file of a managed tool against the disk.
pub fn verify(ctx: &Context, inst: &Installation, tool: &ToolDef) -> Result<Vec<VerifiedFile>> {
    let rec = get_record(&ctx.db, inst.id, tool.id)?
        .ok_or_else(|| Error::NotFound(format!("tool '{}' is not managed by lmm", tool.id)))?;
    let root = target_root(&ctx.paths, inst, tool);
    let mut out = Vec::new();
    for f in files(&ctx.db, rec.row_id)? {
        let target = f.rel.to_native(&root);
        let state = if !target.exists() {
            FileState::Missing
        } else if sha256_file(&target)? != f.sha256 {
            FileState::Modified
        } else {
            FileState::Ok
        };
        out.push(VerifiedFile {
            rel_path: f.rel.as_str().to_string(),
            state,
        });
    }
    Ok(out)
}

/// Outcome of a removal.
#[derive(Debug, Serialize)]
pub struct RemovedTool {
    pub tool_id: String,
    pub removed: usize,
    pub restored: usize,
    /// Files skipped because their content no longer matches the manifest.
    pub skipped: Vec<String>,
}

/// Remove a managed tool: delete files whose content matches the manifest,
/// restore any backed-up originals, drop the record. Drifted files are
/// skipped (reported) unless `force`.
pub fn remove(
    ctx: &Context,
    inst: &Installation,
    tool: &ToolDef,
    force: bool,
) -> Result<RemovedTool> {
    let rec = get_record(&ctx.db, inst.id, tool.id)?
        .ok_or_else(|| Error::NotFound(format!("tool '{}' is not managed by lmm", tool.id)))?;
    let root = target_root(&ctx.paths, inst, tool);
    let mut out = RemovedTool {
        tool_id: tool.id.to_string(),
        removed: 0,
        restored: 0,
        skipped: Vec::new(),
    };
    for f in files(&ctx.db, rec.row_id)? {
        let target = f.rel.to_native(&root);
        match sha256_file(&target) {
            Ok(h) if h == f.sha256 || force => {
                fs::remove_file(&target).path_ctx(&target)?;
                out.removed += 1;
                if let Some(b) = &f.backup_path {
                    let backup_abs = ctx.paths.backups_dir.join(b);
                    if backup_abs.exists() {
                        fs::rename(&backup_abs, &target).path_ctx(&backup_abs)?;
                        out.restored += 1;
                    }
                }
                prune_empty_dirs(&root, &f.rel);
            }
            Ok(_) => out.skipped.push(f.rel.as_str().to_string()),
            // Already gone; nothing to delete.
            Err(_) => {}
        }
    }
    ctx.db
        .conn
        .execute("DELETE FROM tools WHERE id = ?1", [rec.row_id])?;
    // A standalone tool owns its directory; clean it up when now empty.
    if tool.target == Target::Standalone {
        let _ = fs::remove_dir(&root);
    }
    Ok(out)
}

/// Remove now-empty parent directories of `rel` under `root` (best effort;
/// stops at the first non-empty one).
fn prune_empty_dirs(root: &Path, rel: &RelPath) {
    let mut parent = rel.parent();
    while let Some(dir) = parent {
        if fs::remove_dir(dir.to_native(root)).is_err() {
            break;
        }
        parent = dir.parent();
    }
}

/// Is this path currently deployed (owned) by the mod pipeline?
fn deployed_owner(db: &Db, inst_id: i64, path_key: &str) -> Result<bool> {
    // Tool paths are relative to the target root; deployed paths are
    // relative to the game's mod root. They only collide for ModRoot tools,
    // where both roots coincide.
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM deployed_files WHERE installation_id = ?1 AND path_key = ?2",
        params![inst_id, path_key],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Managed tool ids for an installation (for listings and completion).
pub fn managed_ids(db: &Db, inst_id: i64) -> Result<Vec<String>> {
    let mut stmt = db
        .conn
        .prepare("SELECT tool_id FROM tools WHERE installation_id = ?1 ORDER BY tool_id")?;
    let rows = stmt.query_map([inst_id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Descend through solitary wrapper directories ("skse64_2_02_06/...") to
/// the real content root.
///
/// The tool's detect patterns and executable path anchor the search: as
/// soon as one of them matches relative to the current level, that level
/// *is* the content root — this keeps meaningful solitary directories
/// intact (the Address Library archive is nothing but `SKSE/Plugins/...`,
/// which must not be flattened). Without a matching anchor, descending
/// stops at the first level holding any file or more than one entry.
fn unwrap_wrappers(mut root: PathBuf, tool: &ToolDef) -> Result<PathBuf> {
    let anchors: Vec<&str> = tool.detect.iter().copied().chain(tool.exe).collect();
    loop {
        if anchors.iter().any(|a| crate::tools::detect_match(&root, a)) {
            return Ok(root);
        }
        let mut entries = Vec::new();
        for e in fs::read_dir(&root).path_ctx(&root)? {
            entries.push(e.path_ctx(&root)?);
        }
        match &entries[..] {
            [only] if only.file_type().path_ctx(&root)?.is_dir() => {
                root = only.path();
            }
            _ => return Ok(root),
        }
    }
}

/// Best-effort version from an archive filename: the longest run of
/// consecutive numeric tokens ("skse64_2_02_06.7z" -> "2.02.06").
fn guess_version(archive_name: &str) -> Option<String> {
    let stem = archive_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(archive_name);
    let tokens: Vec<&str> = stem.split(['-', '_', ' ']).collect();
    let mut best: Vec<&str> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for t in tokens {
        if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            cur.push(t);
        } else {
            if cur.len() > best.len() {
                best = std::mem::take(&mut cur);
            }
            cur.clear();
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    if best.is_empty() {
        None
    } else {
        Some(best.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_guessing() {
        assert_eq!(
            guess_version("skse64_2_02_06.7z").as_deref(),
            Some("2.02.06")
        );
        assert_eq!(guess_version("xNVSE-6.3.5.zip").as_deref(), Some("6.3.5"));
        assert_eq!(guess_version("LOOT.zip"), None);
        assert_eq!(
            guess_version("SkyUI_5_2_SE-12604-5-2SE.7z").as_deref(),
            Some("5.2")
        );
    }
}
