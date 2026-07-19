//! Deployment: reconcile the game directory with the desired profile state.
//!
//! `plan` is pure inspection: it diffs the desired state against
//! `deployed_files`, stats and hashes the affected targets, and returns every
//! filesystem change `execute` would make, flagging anything that touches
//! content lmm cannot account for (those need `force`). `execute` journals
//! every operation before touching the filesystem and rolls back in reverse
//! on any failure, so the game directory is never left half-deployed.
//! See docs/DESIGN.md §5–6.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Context;
use crate::config::DeployMethod;
use crate::db::{Db, now};
use crate::error::{Error, IoContext, Result};
use crate::games;
use crate::hash::{HashingWriter, sha256_file};
use crate::model::{Deployment, Installation};
use crate::mods;
use crate::paths::RelPath;
use crate::resolve::{self, Provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanKind {
    Deploy,
    Purge,
}

impl PlanKind {
    fn as_str(self) -> &'static str {
        match self {
            PlanKind::Deploy => "deploy",
            PlanKind::Purge => "purge",
        }
    }
}

/// One journal-level filesystem operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    /// Move an unmanaged original out of the way into backups/.
    Backup,
    /// Copy a staged file over the target (temp + rename).
    Write,
    /// Delete a deployed file.
    Remove,
    /// Move a backup back to its original location.
    Restore,
}

impl OpKind {
    fn as_str(self) -> &'static str {
        match self {
            OpKind::Backup => "backup",
            OpKind::Write => "write",
            OpKind::Remove => "remove",
            OpKind::Restore => "restore",
        }
    }
}

#[derive(Debug)]
struct Op {
    kind: OpKind,
    /// Path relative to the mod target root, in the casing this op uses.
    rel: String,
    path_key: String,
    /// Providing mod (Write only).
    mod_id: Option<i64>,
    /// Absolute source: staged file (Write) or backup file (Restore).
    src: Option<PathBuf>,
    /// Expected current hash of the target (Backup/Remove, Write on replace).
    pre_sha256: Option<String>,
    /// Hash of the content present after this op (Write/Restore).
    new_sha256: Option<String>,
    /// Restore: the backups row being restored. Backup: filled at execute.
    backup_id: Option<i64>,
    /// Plan found the target inconsistent with recorded state. Only runs
    /// under `force`, with the strict pre-hash checks relaxed.
    flagged: bool,
}

/// One user-visible change, for display and `--json`.
#[derive(Debug, Serialize)]
pub struct PlanAction {
    /// "install" | "replace" | "remove"
    pub op: &'static str,
    pub rel_path: String,
    pub mod_name: Option<String>,
    /// An unmanaged original at this path is moved into backups/ first.
    pub backs_up_original: bool,
    /// A previously backed-up original is restored after removal.
    pub restores_backup: bool,
    pub warning: Option<String>,
    /// The target differs from recorded state; execution refuses without force.
    pub requires_force: bool,
}

/// deployed_files row to write on commit.
#[derive(Debug)]
struct Upsert {
    path_key: String,
    rel_path: String,
    mod_id: i64,
    sha256: String,
    /// Pre-existing backups row to keep referencing.
    keep_backup: Option<i64>,
    /// Index into `ops` of the Backup that creates this path's backup row.
    backup_op: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub kind: PlanKind,
    pub installation_id: i64,
    pub profile_id: Option<i64>,
    pub actions: Vec<PlanAction>,
    #[serde(skip)]
    ops: Vec<Op>,
    #[serde(skip)]
    upserts: Vec<Upsert>,
    /// path_keys whose deployed_files rows disappear on commit.
    #[serde(skip)]
    deletes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Any action that would destroy content lmm cannot account for.
    pub fn requires_force(&self) -> bool {
        self.actions.iter().any(|a| a.requires_force)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Outcome {
    pub deployment_id: Option<i64>,
    pub installed: usize,
    pub replaced: usize,
    pub removed: usize,
    pub backed_up: usize,
    pub restored: usize,
    /// Written files placed as hard links (deploy.method = "hardlink" and the
    /// staging and game directories share a filesystem); the rest were copied.
    pub hardlinked: usize,
}

/// What `plan` found at a target path.
pub(crate) enum TargetState {
    Missing,
    File(String),
    /// Symlink, directory, or other non-regular file — always unaccounted.
    NonFile(&'static str),
}

pub(crate) fn stat_target(path: &Path) -> Result<TargetState> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TargetState::Missing),
        Err(e) => Err(Error::io(path, e)),
        Ok(md) if md.file_type().is_symlink() => Ok(TargetState::NonFile("symlink")),
        Ok(md) if md.is_dir() => Ok(TargetState::NonFile("directory")),
        Ok(md) if md.is_file() => Ok(TargetState::File(sha256_file(path)?)),
        Ok(_) => Ok(TargetState::NonFile("special file")),
    }
}

/// The directory mod files deploy into (game root + game's mod_root).
pub(crate) fn target_root(inst: &Installation) -> Result<PathBuf> {
    let game = games::by_slug(&inst.game_slug)
        .ok_or_else(|| Error::Invalid(format!("unknown game slug '{}'", inst.game_slug)))?;
    Ok(if game.mod_root.is_empty() {
        inst.path.clone()
    } else {
        inst.path.join(game.mod_root)
    })
}

pub(crate) fn rel_native(root: &Path, rel: &str) -> Result<PathBuf> {
    // rel comes from our own database/plan, but is still re-validated:
    // defense in depth against a tampered database.
    Ok(RelPath::parse(rel)?.to_native(root))
}

// ---------------------------------------------------------------------------
// DB snapshots used by plan

#[derive(Debug)]
struct Have {
    rel_path: String,
    provider_mod_id: i64,
    sha256: String,
    backup_id: Option<i64>,
}

fn load_deployed(db: &Db, inst_id: i64) -> Result<BTreeMap<String, Have>> {
    let mut stmt = db.conn.prepare(
        "SELECT path_key, rel_path, provider_mod_id, sha256, backup_id
         FROM deployed_files WHERE installation_id = ?1",
    )?;
    let rows = stmt.query_map([inst_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Have {
                rel_path: r.get(1)?,
                provider_mod_id: r.get(2)?,
                sha256: r.get(3)?,
                backup_id: r.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Clone)]
struct ModMeta {
    name: String,
    staging_dir: String,
}

fn load_mods_meta(db: &Db, inst_id: i64) -> Result<HashMap<i64, ModMeta>> {
    let mut stmt = db
        .conn
        .prepare("SELECT id, name, staging_dir FROM mods WHERE installation_id = ?1")?;
    let rows = stmt.query_map([inst_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            ModMeta {
                name: r.get(1)?,
                staging_dir: r.get(2)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug)]
struct BackupRow {
    id: i64,
    rel_path: String,
    backup_path: String,
    sha256: String,
}

fn load_backups(db: &Db, inst_id: i64) -> Result<HashMap<String, BackupRow>> {
    let mut stmt = db.conn.prepare(
        "SELECT path_key, id, rel_path, backup_path, sha256
         FROM backups WHERE installation_id = ?1",
    )?;
    let rows = stmt.query_map([inst_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            BackupRow {
                id: r.get(1)?,
                rel_path: r.get(2)?,
                backup_path: r.get(3)?,
                sha256: r.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Absolute path of a mod's staged copy of `rel`; errors if it is missing —
/// deploying from a corrupted staging directory can only make things worse.
fn staged_src(ctx: &Context, meta: &ModMeta, rel: &str) -> Result<PathBuf> {
    let src = rel_native(&ctx.paths.staging_dir.join(&meta.staging_dir), rel)?;
    if !src.is_file() {
        return Err(Error::Blocked(format!(
            "staged file missing for mod '{}': {rel}; reinstall the mod",
            meta.name
        )));
    }
    Ok(src)
}

// ---------------------------------------------------------------------------
// Planning

/// Compute the changes needed to make the game directory match the active
/// profile (Deploy) or the pre-lmm state (Purge). Read-only.
pub fn plan(ctx: &Context, inst: &Installation, kind: PlanKind) -> Result<Plan> {
    ensure_no_running(&ctx.db, inst.id)?;
    let root = target_root(inst)?;

    let (profile_id, desired) = match kind {
        PlanKind::Deploy => {
            let pid = mods::active_profile_id(ctx, inst)?;
            (Some(pid), resolve::desired_state(&ctx.db, pid)?)
        }
        PlanKind::Purge => (None, BTreeMap::new()),
    };
    let deployed = load_deployed(&ctx.db, inst.id)?;
    let mods_meta = load_mods_meta(&ctx.db, inst.id)?;
    let backups = load_backups(&ctx.db, inst.id)?;

    let mut plan = Plan {
        kind,
        installation_id: inst.id,
        profile_id,
        actions: Vec::new(),
        ops: Vec::new(),
        upserts: Vec::new(),
        deletes: Vec::new(),
    };

    let keys: BTreeSet<&String> = desired.keys().chain(deployed.keys()).collect();
    for &key in &keys {
        match (desired.get(key), deployed.get(key)) {
            // Already deployed exactly as desired: nothing to do.
            (Some(w), Some(h))
                if w.mod_id == h.provider_mod_id
                    && w.sha256 == h.sha256
                    && w.rel_path == h.rel_path => {}
            (Some(w), Some(h)) if w.rel_path == h.rel_path => {
                plan_replace(&mut plan, ctx, &root, &mods_meta, key, w, h)?;
            }
            (Some(w), Some(h)) => {
                // Same game path, different on-disk casing: remove the old
                // file, write the new one. Any backup transfers to the new row.
                let (warn, force, _) = plan_remove_ops(&mut plan, ctx, &root, key, h, None)?;
                plan_install(
                    &mut plan,
                    ctx,
                    &root,
                    &mods_meta,
                    key,
                    w,
                    InstallSite {
                        existing_backup: None,
                        transfer_backup: h.backup_id,
                        label: "replace",
                        prior_warning: warn,
                        prior_force: force,
                    },
                )?;
            }
            (Some(w), None) => {
                plan_install(
                    &mut plan,
                    ctx,
                    &root,
                    &mods_meta,
                    key,
                    w,
                    InstallSite {
                        existing_backup: backups.get(key),
                        transfer_backup: None,
                        label: "install",
                        prior_warning: None,
                        prior_force: false,
                    },
                )?;
            }
            (None, Some(h)) => {
                let (warning, requires_force, restores) =
                    plan_remove_ops(&mut plan, ctx, &root, key, h, Some(&backups))?;
                plan.deletes.push(key.clone());
                plan.actions.push(PlanAction {
                    op: "remove",
                    rel_path: h.rel_path.clone(),
                    mod_name: mods_meta.get(&h.provider_mod_id).map(|m| m.name.clone()),
                    backs_up_original: false,
                    restores_backup: restores,
                    warning,
                    requires_force,
                });
            }
            (None, None) => unreachable!("key came from one of the two maps"),
        }
    }
    Ok(plan)
}

struct InstallSite<'a> {
    /// A backups row already exists for this path without a deployed file —
    /// drift from a previous run; never overwrite it.
    existing_backup: Option<&'a BackupRow>,
    /// Re-case replace: the old row's backup carries over to the new row.
    transfer_backup: Option<i64>,
    label: &'static str,
    prior_warning: Option<String>,
    prior_force: bool,
}

fn plan_install(
    plan: &mut Plan,
    ctx: &Context,
    root: &Path,
    mods_meta: &HashMap<i64, ModMeta>,
    key: &str,
    w: &Provider,
    site: InstallSite<'_>,
) -> Result<()> {
    let meta = mods_meta
        .get(&w.mod_id)
        .ok_or_else(|| Error::NotFound(format!("mod {} for {}", w.mod_id, w.rel_path)))?;
    let src = staged_src(ctx, meta, &w.rel_path)?;
    let target = rel_native(root, &w.rel_path)?;

    let mut warning = site.prior_warning;
    let mut requires_force = site.prior_force;
    let mut flagged = false;
    let mut keep_backup = site.transfer_backup;
    let mut backup_op = None;

    match stat_target(&target)? {
        TargetState::Missing => {}
        TargetState::NonFile(kind) => {
            warning = merge_warn(warning, format!("target is a {kind} lmm did not create"));
            requires_force = true;
            flagged = true;
        }
        TargetState::File(sha) => {
            if let Some(b) = site.existing_backup {
                // The original is already safe in backups/; the file on disk
                // now is unaccounted content from outside lmm.
                warning = merge_warn(
                    warning,
                    "unrecognized file at target (original already backed up)".into(),
                );
                requires_force = true;
                flagged = true;
                keep_backup = keep_backup.or(Some(b.id));
            } else if site.transfer_backup.is_some() {
                // Re-case replace, but a file also exists at the new casing.
                warning = merge_warn(warning, "unrecognized file at target".into());
                requires_force = true;
                flagged = true;
            } else {
                // An original game file: move it to backups/ before writing.
                plan.ops.push(Op {
                    kind: OpKind::Backup,
                    rel: w.rel_path.clone(),
                    path_key: key.to_string(),
                    mod_id: None,
                    src: None,
                    pre_sha256: Some(sha),
                    new_sha256: None,
                    backup_id: None,
                    flagged: false,
                });
                backup_op = Some(plan.ops.len() - 1);
            }
        }
    }

    plan.ops.push(Op {
        kind: OpKind::Write,
        rel: w.rel_path.clone(),
        path_key: key.to_string(),
        mod_id: Some(w.mod_id),
        src: Some(src),
        pre_sha256: None,
        new_sha256: Some(w.sha256.clone()),
        backup_id: None,
        flagged,
    });
    plan.upserts.push(Upsert {
        path_key: key.to_string(),
        rel_path: w.rel_path.clone(),
        mod_id: w.mod_id,
        sha256: w.sha256.clone(),
        keep_backup,
        backup_op,
    });
    plan.actions.push(PlanAction {
        op: site.label,
        rel_path: w.rel_path.clone(),
        mod_name: Some(w.mod_name.clone()),
        backs_up_original: backup_op.is_some(),
        restores_backup: false,
        warning,
        requires_force,
    });
    Ok(())
}

fn plan_replace(
    plan: &mut Plan,
    ctx: &Context,
    root: &Path,
    mods_meta: &HashMap<i64, ModMeta>,
    key: &str,
    w: &Provider,
    h: &Have,
) -> Result<()> {
    let meta = mods_meta
        .get(&w.mod_id)
        .ok_or_else(|| Error::NotFound(format!("mod {} for {}", w.mod_id, w.rel_path)))?;
    let src = staged_src(ctx, meta, &w.rel_path)?;
    let target = rel_native(root, &w.rel_path)?;

    let (warning, requires_force, flagged) = match stat_target(&target)? {
        // Someone deleted our file; writing it back is what deploy is for.
        TargetState::Missing => (Some("was removed outside lmm".into()), false, false),
        TargetState::NonFile(kind) => (
            Some(format!("target is a {kind} lmm did not create")),
            true,
            true,
        ),
        TargetState::File(sha) if sha != h.sha256 => {
            (Some("modified outside lmm".into()), true, true)
        }
        TargetState::File(_) => (None, false, false),
    };

    plan.ops.push(Op {
        kind: OpKind::Write,
        rel: w.rel_path.clone(),
        path_key: key.to_string(),
        mod_id: Some(w.mod_id),
        src: Some(src),
        pre_sha256: Some(h.sha256.clone()),
        new_sha256: Some(w.sha256.clone()),
        backup_id: None,
        flagged,
    });
    plan.upserts.push(Upsert {
        path_key: key.to_string(),
        rel_path: w.rel_path.clone(),
        mod_id: w.mod_id,
        sha256: w.sha256.clone(),
        keep_backup: h.backup_id,
        backup_op: None,
    });
    plan.actions.push(PlanAction {
        op: "replace",
        rel_path: w.rel_path.clone(),
        mod_name: Some(w.mod_name.clone()),
        backs_up_original: false,
        restores_backup: false,
        warning,
        requires_force,
    });
    Ok(())
}

/// Emit Remove (and, when `backups` is given and the row has one, Restore)
/// ops for a deployed file. Returns (warning, requires_force, restores).
fn plan_remove_ops(
    plan: &mut Plan,
    ctx: &Context,
    root: &Path,
    key: &str,
    h: &Have,
    backups: Option<&HashMap<String, BackupRow>>,
) -> Result<(Option<String>, bool, bool)> {
    let target = rel_native(root, &h.rel_path)?;
    let (mut warning, mut requires_force, flagged) = match stat_target(&target)? {
        // Deleting an already-deleted file is a no-op, not a danger.
        TargetState::Missing => (Some("already removed outside lmm".into()), false, false),
        TargetState::NonFile(kind) => (
            Some(format!("target is a {kind} lmm did not create")),
            true,
            true,
        ),
        TargetState::File(sha) if sha != h.sha256 => {
            (Some("modified outside lmm".into()), true, true)
        }
        TargetState::File(_) => (None, false, false),
    };

    plan.ops.push(Op {
        kind: OpKind::Remove,
        rel: h.rel_path.clone(),
        path_key: key.to_string(),
        mod_id: None,
        src: None,
        pre_sha256: Some(h.sha256.clone()),
        new_sha256: None,
        backup_id: None,
        flagged,
    });

    let mut restores = false;
    if let (Some(backups), Some(backup_id)) = (backups, h.backup_id) {
        // FK guarantees the row exists; be defensive anyway.
        match backups.get(key) {
            Some(b) if backup_file(ctx, &b.backup_path).is_ok_and(|p| p.is_file()) => {
                plan.ops.push(Op {
                    kind: OpKind::Restore,
                    rel: b.rel_path.clone(),
                    path_key: key.to_string(),
                    mod_id: None,
                    src: backup_file(ctx, &b.backup_path).ok(),
                    pre_sha256: None,
                    new_sha256: Some(b.sha256.clone()),
                    backup_id: Some(b.id),
                    flagged: false,
                });
                restores = true;
            }
            _ => {
                // The backup vanished: the original cannot come back. Under
                // force the restore is skipped and the row dropped.
                warning = merge_warn(
                    warning,
                    "backup file missing; original cannot be restored".into(),
                );
                requires_force = true;
                plan.ops.push(Op {
                    kind: OpKind::Restore,
                    rel: h.rel_path.clone(),
                    path_key: key.to_string(),
                    mod_id: None,
                    src: None,
                    pre_sha256: None,
                    new_sha256: None,
                    backup_id: Some(backup_id),
                    flagged: true,
                });
            }
        }
    }
    Ok((warning, requires_force, restores))
}

fn merge_warn(existing: Option<String>, new: String) -> Option<String> {
    Some(match existing {
        Some(w) => format!("{w}; {new}"),
        None => new,
    })
}

fn backup_file(ctx: &Context, backup_path: &str) -> Result<PathBuf> {
    rel_native(&ctx.paths.backups_dir, backup_path)
}

// ---------------------------------------------------------------------------
// Execution

/// Apply a plan. Journals every operation before running it; on any failure
/// all completed operations are undone in reverse and the error is returned.
pub fn execute(ctx: &Context, inst: &Installation, mut plan: Plan, force: bool) -> Result<Outcome> {
    if plan.ops.is_empty() {
        return Ok(Outcome::default());
    }
    if plan.requires_force() && !force {
        let n = plan.actions.iter().filter(|a| a.requires_force).count();
        return Err(Error::Blocked(format!(
            "{n} target file(s) differ from recorded state (see the plan); pass --force to override"
        )));
    }
    ensure_no_running(&ctx.db, inst.id)?;

    let root = target_root(inst)?;
    fs::create_dir_all(&root).path_ctx(&root)?;
    let canon_root = root.canonicalize().path_ctx(&root)?;

    // Durable intent log before any filesystem change.
    let deployment_id = {
        let tx = ctx.db.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO deployments (installation_id, profile_id, kind, status, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![inst.id, plan.profile_id, plan.kind.as_str(), now()],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO journal (deployment_id, seq, op, rel_path, path_key, mod_id,
                                      backup_id, pre_sha256, new_sha256, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending')",
            )?;
            for (i, op) in plan.ops.iter().enumerate() {
                stmt.execute(params![
                    id,
                    (i + 1) as i64,
                    op.kind.as_str(),
                    op.rel,
                    op.path_key,
                    op.mod_id,
                    op.backup_id,
                    op.pre_sha256,
                    op.new_sha256,
                ])?;
            }
        }
        tx.commit()?;
        id
    };

    match run_ops(
        ctx,
        inst,
        &mut plan,
        deployment_id,
        &root,
        &canon_root,
        force,
    ) {
        Ok(hardlinked) => {
            commit(ctx, inst, &plan, deployment_id)?;
            Ok(Outcome {
                deployment_id: Some(deployment_id),
                installed: plan.actions.iter().filter(|a| a.op == "install").count(),
                replaced: plan.actions.iter().filter(|a| a.op == "replace").count(),
                removed: plan.actions.iter().filter(|a| a.op == "remove").count(),
                backed_up: plan.ops.iter().filter(|o| o.kind == OpKind::Backup).count(),
                restored: plan
                    .ops
                    .iter()
                    .filter(|o| o.kind == OpKind::Restore && !o.flagged)
                    .count(),
                hardlinked,
            })
        }
        Err(e) => match rollback_deployment(ctx, inst, deployment_id) {
            Ok(()) => Err(Error::Deploy(format!(
                "{}: {e}; all changes were rolled back",
                plan.kind.as_str()
            ))),
            Err(re) => Err(Error::Deploy(format!(
                "{} failed ({e}) and rollback also failed ({re}); \
                 the deployment is left open — run 'lmm rollback'",
                plan.kind.as_str()
            ))),
        },
    }
}

/// Returns how many Write ops were placed as hard links.
#[allow(clippy::too_many_arguments)]
fn run_ops(
    ctx: &Context,
    inst: &Installation,
    plan: &mut Plan,
    deployment_id: i64,
    root: &Path,
    canon_root: &Path,
    force: bool,
) -> Result<usize> {
    let mut checked_dirs: HashSet<PathBuf> = HashSet::new();
    let mut removed_targets: Vec<PathBuf> = Vec::new();
    let mut hardlinked = 0usize;

    for (i, op) in plan.ops.iter_mut().enumerate() {
        let seq = (i + 1) as i64;
        let target = rel_native(root, &op.rel)?;
        let relaxed = op.flagged && force;
        match op.kind {
            OpKind::Backup => {
                let cur = sha256_file(&target)?;
                if !relaxed && op.pre_sha256.as_deref() != Some(cur.as_str()) {
                    return Err(Error::Deploy(format!(
                        "{}: changed between plan and execute",
                        op.rel
                    )));
                }
                check_containment(canon_root, &target, false, &mut checked_dirs)?;
                let backup_rel = format!("{}/{}", inst.id, op.rel);
                let bpath = rel_native(&ctx.paths.backups_dir, &backup_rel)?;
                if bpath.exists() {
                    // Never overwrite an existing backup.
                    return Err(Error::Deploy(format!(
                        "backup destination already exists: {}",
                        bpath.display()
                    )));
                }
                move_file(&target, &bpath)?;
                ctx.db.conn.execute(
                    "INSERT INTO backups (installation_id, path_key, rel_path, backup_path,
                                          sha256, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![inst.id, op.path_key, op.rel, backup_rel, cur, now()],
                )?;
                op.backup_id = Some(ctx.db.conn.last_insert_rowid());
            }
            OpKind::Write => {
                match (op.pre_sha256.as_deref(), target.is_file()) {
                    // A file appeared where the plan saw none.
                    (None, true) if !relaxed => {
                        return Err(Error::Deploy(format!(
                            "{}: appeared between plan and execute",
                            op.rel
                        )));
                    }
                    (Some(pre), true) if !relaxed => {
                        let cur = sha256_file(&target)?;
                        if cur != pre {
                            return Err(Error::Deploy(format!(
                                "{}: modified outside lmm (re-run 'lmm deploy --dry-run')",
                                op.rel
                            )));
                        }
                    }
                    _ => {}
                }
                check_containment(canon_root, &target, true, &mut checked_dirs)?;
                let src = op.src.as_deref().ok_or_else(|| {
                    Error::Invalid(format!("write op for {} has no source", op.rel))
                })?;
                let new = op.new_sha256.as_deref().ok_or_else(|| {
                    Error::Invalid(format!("write op for {} has no hash", op.rel))
                })?;
                if place_file(ctx.config.deploy.method, src, &target, new)? {
                    hardlinked += 1;
                }
            }
            OpKind::Remove => {
                if target.is_file() {
                    if !relaxed {
                        let cur = sha256_file(&target)?;
                        if op.pre_sha256.as_deref() != Some(cur.as_str()) {
                            return Err(Error::Deploy(format!(
                                "{}: modified outside lmm (re-run 'lmm deploy --dry-run')",
                                op.rel
                            )));
                        }
                    }
                    check_containment(canon_root, &target, false, &mut checked_dirs)?;
                    fs::remove_file(&target).path_ctx(&target)?;
                    removed_targets.push(target);
                }
                // Already missing: removal is idempotent drift-tolerance.
            }
            OpKind::Restore => {
                if op.flagged {
                    // Backup file is gone; under force the restore is skipped
                    // and commit drops the row.
                    journal_done(&ctx.db, deployment_id, seq, None)?;
                    continue;
                }
                let src = op.src.as_deref().ok_or_else(|| {
                    Error::Invalid(format!("restore op for {} has no source", op.rel))
                })?;
                let expect = op.new_sha256.as_deref().unwrap_or_default();
                let cur = sha256_file(src)?;
                if cur != expect {
                    return Err(Error::Deploy(format!(
                        "backup of {} does not match its recorded hash",
                        op.rel
                    )));
                }
                if target.exists() {
                    return Err(Error::Deploy(format!(
                        "{}: cannot restore backup, target exists",
                        op.rel
                    )));
                }
                check_containment(canon_root, &target, true, &mut checked_dirs)?;
                move_file(src, &target)?;
            }
        }
        journal_done(&ctx.db, deployment_id, seq, op.backup_id)?;
    }

    // Prune directory chains emptied by removals (restores keep theirs full).
    for target in &removed_targets {
        prune_empty_dirs(canon_root, target);
    }
    Ok(hardlinked)
}

fn journal_done(db: &Db, deployment_id: i64, seq: i64, backup_id: Option<i64>) -> Result<()> {
    db.conn.execute(
        "UPDATE journal SET state = 'done', backup_id = COALESCE(?3, backup_id)
         WHERE deployment_id = ?1 AND seq = ?2",
        params![deployment_id, seq, backup_id],
    )?;
    Ok(())
}

/// One transaction: deployed_files reflects the new reality, restored backup
/// rows disappear, the deployment is committed. DB state therefore only ever
/// describes fully-applied deployments.
fn commit(ctx: &Context, inst: &Installation, plan: &Plan, deployment_id: i64) -> Result<()> {
    let tx = ctx.db.conn.unchecked_transaction()?;
    for key in &plan.deletes {
        tx.execute(
            "DELETE FROM deployed_files WHERE installation_id = ?1 AND path_key = ?2",
            params![inst.id, key],
        )?;
    }
    for up in &plan.upserts {
        let backup_id = up
            .keep_backup
            .or_else(|| up.backup_op.and_then(|i| plan.ops[i].backup_id));
        tx.execute(
            "INSERT INTO deployed_files (installation_id, path_key, rel_path,
                                         provider_mod_id, sha256, backup_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(installation_id, path_key) DO UPDATE SET
                rel_path = excluded.rel_path,
                provider_mod_id = excluded.provider_mod_id,
                sha256 = excluded.sha256,
                backup_id = excluded.backup_id",
            params![
                inst.id,
                up.path_key,
                up.rel_path,
                up.mod_id,
                up.sha256,
                backup_id
            ],
        )?;
    }
    for op in &plan.ops {
        if op.kind == OpKind::Restore
            && let Some(id) = op.backup_id
        {
            tx.execute("DELETE FROM backups WHERE id = ?1", [id])?;
        }
    }
    tx.execute(
        "UPDATE deployments SET status = 'committed', finished_at = ?2 WHERE id = ?1",
        params![deployment_id, now()],
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rollback

fn row_to_deployment(r: &rusqlite::Row<'_>) -> rusqlite::Result<Deployment> {
    Ok(Deployment {
        id: r.get(0)?,
        installation_id: r.get(1)?,
        profile_id: r.get(2)?,
        kind: r.get(3)?,
        status: r.get(4)?,
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
    })
}

const SELECT_DEPLOYMENT: &str = "
    SELECT id, installation_id, profile_id, kind, status, started_at, finished_at
    FROM deployments WHERE installation_id = ?1";

/// The pending (crashed or in-flight) deployment of an installation, if any.
pub fn find_running(db: &Db, inst_id: i64) -> Result<Option<Deployment>> {
    Ok(db
        .conn
        .query_row(
            &format!("{SELECT_DEPLOYMENT} AND status = 'running' ORDER BY id LIMIT 1"),
            [inst_id],
            row_to_deployment,
        )
        .optional()?)
}

/// The most recent deployment of an installation, whatever its outcome.
pub fn last(db: &Db, inst_id: i64) -> Result<Option<Deployment>> {
    Ok(db
        .conn
        .query_row(
            &format!("{SELECT_DEPLOYMENT} ORDER BY id DESC LIMIT 1"),
            [inst_id],
            row_to_deployment,
        )
        .optional()?)
}

pub(crate) fn ensure_no_running(db: &Db, inst_id: i64) -> Result<()> {
    if let Some(d) = find_running(db, inst_id)? {
        return Err(Error::Blocked(format!(
            "an interrupted {} (id {}) is pending for this installation; \
             run 'lmm rollback' to recover",
            d.kind, d.id
        )));
    }
    Ok(())
}

/// Roll back the pending deployment of an installation, if there is one.
/// Returns the rolled-back deployment id.
pub fn rollback_running(ctx: &Context, inst: &Installation) -> Result<Option<i64>> {
    match find_running(&ctx.db, inst.id)? {
        None => Ok(None),
        Some(d) => {
            rollback_deployment(ctx, inst, d.id)?;
            Ok(Some(d.id))
        }
    }
}

struct JournalRow {
    seq: i64,
    op: String,
    rel: String,
    path_key: String,
    backup_id: Option<i64>,
    new_sha256: Option<String>,
}

/// Undo every completed journal op of a deployment, newest first, verifying
/// each step against the filesystem. Idempotent: undone rows are skipped, and
/// each undo checks current state before acting, so it is safe to re-run
/// after a crash mid-rollback.
fn rollback_deployment(ctx: &Context, inst: &Installation, deployment_id: i64) -> Result<()> {
    let root = target_root(inst)?;
    let canon_root = root.canonicalize().path_ctx(&root)?;

    let rows: Vec<JournalRow> = {
        let mut stmt = ctx.db.conn.prepare(
            "SELECT seq, op, rel_path, path_key, backup_id, new_sha256
             FROM journal WHERE deployment_id = ?1 AND state = 'done'
             ORDER BY seq DESC",
        )?;
        let rows = stmt.query_map([deployment_id], |r| {
            Ok(JournalRow {
                seq: r.get(0)?,
                op: r.get(1)?,
                rel: r.get(2)?,
                path_key: r.get(3)?,
                backup_id: r.get(4)?,
                new_sha256: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut pruned: Vec<PathBuf> = Vec::new();
    for row in rows {
        let target = rel_native(&root, &row.rel)?;
        match row.op.as_str() {
            "write" => {
                let tmp = tmp_path(&target);
                if tmp.exists() {
                    let _ = fs::remove_file(&tmp);
                }
                // Delete only if the file is still the one we wrote.
                if target.is_file() && Some(sha256_file(&target)?) == row.new_sha256 {
                    fs::remove_file(&target).path_ctx(&target)?;
                    pruned.push(target.clone());
                }
                restore_prior_content(ctx, inst, &root, &row.path_key)?;
            }
            "remove" => {
                restore_prior_content(ctx, inst, &root, &row.path_key)?;
            }
            "backup" => {
                if let Some(bid) = row.backup_id
                    && let Some(b) = backup_by_id(&ctx.db, bid)?
                {
                    let bfile = backup_file(ctx, &b.backup_path)?;
                    if !target.exists() && bfile.is_file() {
                        move_file(&bfile, &target)?;
                    }
                    ctx.db
                        .conn
                        .execute("DELETE FROM backups WHERE id = ?1", [bid])?;
                }
            }
            "restore" => {
                // Undo a restore: move the original back into backups/,
                // unless commit already deleted the row (then we never get
                // here — committed deployments are not rolled back).
                if let Some(bid) = row.backup_id
                    && let Some(b) = backup_by_id(&ctx.db, bid)?
                {
                    let bfile = backup_file(ctx, &b.backup_path)?;
                    if !bfile.exists() && target.is_file() && sha256_file(&target)? == b.sha256 {
                        move_file(&target, &bfile)?;
                        pruned.push(target.clone());
                    }
                }
            }
            other => {
                return Err(Error::Invalid(format!("unknown journal op '{other}'")));
            }
        }
        ctx.db.conn.execute(
            "UPDATE journal SET state = 'undone' WHERE deployment_id = ?1 AND seq = ?2",
            params![deployment_id, row.seq],
        )?;
    }

    for target in &pruned {
        prune_empty_dirs(&canon_root, target);
    }
    ctx.db.conn.execute(
        "UPDATE deployments SET status = 'rolled_back', finished_at = ?2 WHERE id = ?1",
        params![deployment_id, now()],
    )?;
    Ok(())
}

/// Re-copy the content `deployed_files` says should be at `path_key` (the
/// table still holds the pre-deployment state until commit). No row = the
/// path was empty before; nothing to restore.
fn restore_prior_content(
    ctx: &Context,
    inst: &Installation,
    root: &Path,
    path_key: &str,
) -> Result<()> {
    let prior: Option<(String, String, String)> = ctx
        .db
        .conn
        .query_row(
            "SELECT d.rel_path, d.sha256, m.staging_dir
             FROM deployed_files d JOIN mods m ON m.id = d.provider_mod_id
             WHERE d.installation_id = ?1 AND d.path_key = ?2",
            params![inst.id, path_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((rel, sha, staging_dir)) = prior {
        let target = rel_native(root, &rel)?;
        if !target.exists() {
            let src = rel_native(&ctx.paths.staging_dir.join(&staging_dir), &rel)?;
            place_file(ctx.config.deploy.method, &src, &target, &sha)?;
        }
    }
    Ok(())
}

fn backup_by_id(db: &Db, id: i64) -> Result<Option<BackupRow>> {
    Ok(db
        .conn
        .query_row(
            "SELECT id, rel_path, backup_path, sha256 FROM backups WHERE id = ?1",
            [id],
            |r| {
                Ok(BackupRow {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    backup_path: r.get(2)?,
                    sha256: r.get(3)?,
                })
            },
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// Filesystem primitives

fn tmp_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lmm-tmp");
    target.with_file_name(name)
}

/// Copy `src` to `dst` via a temp file in the same directory: hash-verified
/// against `expect_sha256`, fsynced, then renamed over the target. A crash
/// mid-copy leaves only a `.lmm-tmp` file, never a truncated target.
pub(crate) fn copy_atomic(src: &Path, dst: &Path, expect_sha256: &str) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).path_ctx(parent)?;
    }
    let tmp = tmp_path(dst);
    let result = (|| -> Result<()> {
        let mut reader = File::open(src).path_ctx(src)?;
        let mut writer = HashingWriter::new(File::create(&tmp).path_ctx(&tmp)?);
        std::io::copy(&mut reader, &mut writer).path_ctx(src)?;
        let (file, hash, _) = writer.finish();
        if hash != expect_sha256 {
            return Err(Error::Deploy(format!(
                "{} does not match its recorded hash",
                src.display()
            )));
        }
        file.sync_all().path_ctx(&tmp)?;
        fs::rename(&tmp, dst).path_ctx(dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Place `src`'s content at `dst` per the configured method. Returns true
/// when the file was hard-linked (target shares storage with staging).
///
/// Hardlink placement verifies the staged file against `expect_sha256`
/// first (a mismatch is corruption and never falls back), then links to a
/// temp name and renames over the target — same atomicity as the copy
/// path. Any link failure (different filesystem, filesystem without
/// hardlink support, exhausted link count) falls back to a verified copy:
/// the copy either succeeds or produces the authoritative error.
pub(crate) fn place_file(
    method: DeployMethod,
    src: &Path,
    dst: &Path,
    expect_sha256: &str,
) -> Result<bool> {
    if method == DeployMethod::Hardlink {
        if sha256_file(src)? != expect_sha256 {
            return Err(Error::Deploy(format!(
                "{} does not match its recorded hash",
                src.display()
            )));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).path_ctx(parent)?;
        }
        let tmp = tmp_path(dst);
        let _ = fs::remove_file(&tmp);
        if fs::hard_link(src, &tmp).is_ok() {
            match fs::rename(&tmp, dst) {
                Ok(()) => return Ok(true),
                Err(_) => {
                    let _ = fs::remove_file(&tmp);
                }
            }
        }
    }
    copy_atomic(src, dst, expect_sha256)?;
    Ok(false)
}

/// Move a file, falling back to verified copy + delete across filesystems
/// (the game library and lmm's data dir are often on different disks).
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).path_ctx(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let sha = sha256_file(src)?;
            copy_atomic(src, dst, &sha)?;
            fs::remove_file(src).path_ctx(src)
        }
        Err(e) => Err(Error::io(src, e)),
    }
}

/// Verify that `target`'s parent directory really resolves inside the game
/// root — a symlinked subdirectory must not smuggle writes out of the tree.
pub(crate) fn check_containment(
    canon_root: &Path,
    target: &Path,
    create: bool,
    checked: &mut HashSet<PathBuf>,
) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::Invalid(format!("{}: no parent directory", target.display())))?;
    if checked.contains(parent) {
        return Ok(());
    }
    if create {
        fs::create_dir_all(parent).path_ctx(parent)?;
    }
    let canon = parent.canonicalize().path_ctx(parent)?;
    if !canon.starts_with(canon_root) {
        return Err(Error::Deploy(format!(
            "refusing to touch {}: it resolves outside the game directory",
            target.display()
        )));
    }
    checked.insert(parent.to_path_buf());
    Ok(())
}

/// Best-effort removal of now-empty directories between a removed file and
/// the target root, so purge leaves no skeleton behind.
fn prune_empty_dirs(canon_root: &Path, target: &Path) {
    let mut dir = target.parent();
    while let Some(d) = dir {
        match d.canonicalize() {
            Ok(c) if c != canon_root && c.starts_with(canon_root) => {
                if fs::remove_dir(d).is_err() {
                    break; // not empty (or gone): stop climbing
                }
            }
            _ => break,
        }
        dir = d.parent();
    }
}
