//! Verify and repair: database ↔ filesystem drift.
//!
//! The database is the source of truth for *managed* state, the filesystem
//! for *actual* state. `report` re-hashes everything the database claims —
//! deployed files in the game directory, the backup store, and staging — and
//! lists every mismatch. `plan_repair`/`execute_repair` converge the
//! filesystem back to the recorded state: deployed files are re-copied from
//! staging, broken staged files are recovered from intact deployed copies.
//! Repair writes are per-file atomic and the whole operation is idempotent
//! (it only ever moves the filesystem *toward* the database), so unlike
//! deployment it needs no journal: an interrupted repair is fixed by
//! repairing again.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::Context;
use crate::deploy::{self, TargetState};
use crate::error::{Error, IoContext, Result};
use crate::hash::sha256_file;
use crate::model::Installation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Problem {
    InterruptedDeployment,
    GameDirMissing,
    DeployedMissing,
    DeployedModified,
    DeployedTypeChanged,
    BackupMissing,
    BackupModified,
    StagedMissing,
    StagedModified,
}

impl Problem {
    pub fn describe(self) -> &'static str {
        match self {
            Problem::InterruptedDeployment => "interrupted deployment pending",
            Problem::GameDirMissing => "game directory missing or moved",
            Problem::DeployedMissing => "deployed file missing",
            Problem::DeployedModified => "deployed file modified outside lmm",
            Problem::DeployedTypeChanged => "deployed file replaced by another kind of entry",
            Problem::BackupMissing => "backup file missing",
            Problem::BackupModified => "backup file does not match its recorded hash",
            Problem::StagedMissing => "staged file missing",
            Problem::StagedModified => "staged file does not match its recorded hash",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub problem: Problem,
    pub rel_path: String,
    pub mod_name: Option<String>,
    pub detail: Option<String>,
    #[serde(skip)]
    path_key: String,
    /// Provider (deployed findings) or owner (staged findings).
    #[serde(skip)]
    mod_id: Option<i64>,
    /// The hash the database records for this file.
    #[serde(skip)]
    expect_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub installation_id: i64,
    pub checked_deployed: usize,
    pub checked_backups: usize,
    pub checked_staged: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Re-hash everything the database claims about this installation and report
/// every divergence. Read-only.
pub fn report(ctx: &Context, inst: &Installation) -> Result<Report> {
    let mut findings = Vec::new();

    if let Some(d) = deploy::find_running(&ctx.db, inst.id)? {
        findings.push(Finding {
            problem: Problem::InterruptedDeployment,
            rel_path: String::new(),
            mod_name: None,
            detail: Some(format!("{} (id {}); run 'lmm rollback'", d.kind, d.id)),
            path_key: String::new(),
            mod_id: None,
            expect_sha256: None,
        });
    }

    // Deployed files. A missing game root would make every row "missing";
    // one clear finding beats a thousand noisy ones.
    let mut checked_deployed = 0;
    if inst.path.is_dir() {
        let root = deploy::target_root(inst)?;
        let mod_names = mod_names(ctx, inst.id)?;
        for row in deployed_rows(ctx, inst.id)? {
            checked_deployed += 1;
            let target = deploy::rel_native(&root, &row.rel_path)?;
            let (problem, detail) = match deploy::stat_target(&target)? {
                TargetState::File(sha) if sha == row.sha256 => continue,
                TargetState::File(_) => (Problem::DeployedModified, None),
                TargetState::Missing => (Problem::DeployedMissing, None),
                TargetState::NonFile(kind) => {
                    (Problem::DeployedTypeChanged, Some(kind.to_string()))
                }
            };
            findings.push(Finding {
                problem,
                rel_path: row.rel_path,
                mod_name: mod_names.get(&row.provider_mod_id).cloned(),
                detail,
                path_key: row.path_key,
                mod_id: Some(row.provider_mod_id),
                expect_sha256: Some(row.sha256),
            });
        }
    } else {
        findings.push(Finding {
            problem: Problem::GameDirMissing,
            rel_path: inst.path.to_string_lossy().into_owned(),
            mod_name: None,
            detail: None,
            path_key: String::new(),
            mod_id: None,
            expect_sha256: None,
        });
    }

    // Backup store (lives in the data dir; checkable even without the game).
    let mut checked_backups = 0;
    {
        let mut stmt = ctx.db.conn.prepare(
            "SELECT path_key, rel_path, backup_path, sha256
             FROM backups WHERE installation_id = ?1",
        )?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map([inst.id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (path_key, rel_path, backup_path, sha256) in rows {
            checked_backups += 1;
            let file = deploy::rel_native(&ctx.paths.backups_dir, &backup_path)?;
            let problem = if !file.is_file() {
                Problem::BackupMissing
            } else if sha256_file(&file)? != sha256 {
                Problem::BackupModified
            } else {
                continue;
            };
            findings.push(Finding {
                problem,
                rel_path,
                mod_name: None,
                detail: None,
                path_key,
                mod_id: None,
                expect_sha256: Some(sha256),
            });
        }
    }

    // Staging: the canonical copy of every installed mod, deployed or not.
    let mut checked_staged = 0;
    {
        let mut stmt = ctx.db.conn.prepare(
            "SELECT m.id, m.name, m.staging_dir, f.rel_path, f.path_key, f.sha256
             FROM mods m JOIN mod_files f ON f.mod_id = m.id
             WHERE m.installation_id = ?1 ORDER BY m.id, f.path_key",
        )?;
        let rows: Vec<(i64, String, String, String, String, String)> = stmt
            .query_map([inst.id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (mod_id, name, staging_dir, rel_path, path_key, sha256) in rows {
            checked_staged += 1;
            let file = deploy::rel_native(&ctx.paths.staging_dir.join(&staging_dir), &rel_path)?;
            let problem = if !file.is_file() {
                Problem::StagedMissing
            } else if sha256_file(&file)? != sha256 {
                Problem::StagedModified
            } else {
                continue;
            };
            findings.push(Finding {
                problem,
                rel_path,
                mod_name: Some(name),
                detail: None,
                path_key,
                mod_id: Some(mod_id),
                expect_sha256: Some(sha256),
            });
        }
    }

    Ok(Report {
        installation_id: inst.id,
        checked_deployed,
        checked_backups,
        checked_staged,
        findings,
    })
}

// ---------------------------------------------------------------------------
// Repair

#[derive(Debug)]
enum Fix {
    /// Re-copy a deployed file from its provider's staging.
    Recopy {
        src: PathBuf,
        target: PathBuf,
        sha256: String,
        /// The target is a symlink that must be removed first.
        unlink: bool,
    },
    /// Rebuild a staged file from the intact deployed copy on disk.
    RecoverStaged {
        from: PathBuf,
        to: PathBuf,
        sha256: String,
    },
}

#[derive(Debug, Serialize)]
pub struct RepairAction {
    /// "recopy" | "recover-staging" | "skip"
    pub op: &'static str,
    pub rel_path: String,
    pub mod_name: Option<String>,
    /// Fixing would overwrite content lmm cannot account for.
    pub requires_force: bool,
    pub note: Option<String>,
    #[serde(skip)]
    fix: Option<Fix>,
}

#[derive(Debug, Serialize)]
pub struct RepairPlan {
    pub installation_id: i64,
    pub actions: Vec<RepairAction>,
}

impl RepairPlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn requires_force(&self) -> bool {
        self.actions.iter().any(|a| a.requires_force)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct RepairOutcome {
    pub repaired: usize,
    /// Fixes that need --force and were skipped without it.
    pub skipped_force: usize,
    /// Nothing to repair from: the intact copy no longer exists anywhere.
    pub unrepairable: usize,
}

/// Turn a drift report into concrete fixes. Refuses while the installation
/// is in a state repair cannot reason about (pending deployment, moved game).
pub fn plan_repair(ctx: &Context, inst: &Installation) -> Result<RepairPlan> {
    let rep = report(ctx, inst)?;
    if rep
        .findings
        .iter()
        .any(|f| f.problem == Problem::InterruptedDeployment)
    {
        return Err(Error::Blocked(
            "an interrupted deployment is pending; run 'lmm rollback' first".into(),
        ));
    }
    if rep
        .findings
        .iter()
        .any(|f| f.problem == Problem::GameDirMissing)
    {
        return Err(Error::Blocked(format!(
            "game directory {} is missing; restore it or re-register the installation",
            inst.path.display()
        )));
    }

    let root = deploy::target_root(inst)?;
    let staging_dirs = staging_dirs(ctx, inst.id)?;
    let deployed: HashMap<String, DeployedRow> = deployed_rows(ctx, inst.id)?
        .into_iter()
        .map(|r| (r.path_key.clone(), r))
        .collect();
    // A broken staged file cannot repair its deployed copy, and vice versa.
    let broken_staged: HashSet<(i64, &str)> = rep
        .findings
        .iter()
        .filter(|f| matches!(f.problem, Problem::StagedMissing | Problem::StagedModified))
        .filter_map(|f| f.mod_id.map(|id| (id, f.path_key.as_str())))
        .collect();
    let broken_deployed: HashSet<&str> = rep
        .findings
        .iter()
        .filter(|f| {
            matches!(
                f.problem,
                Problem::DeployedMissing | Problem::DeployedModified | Problem::DeployedTypeChanged
            )
        })
        .map(|f| f.path_key.as_str())
        .collect();

    let mut actions = Vec::new();
    for f in &rep.findings {
        match f.problem {
            Problem::DeployedMissing | Problem::DeployedModified | Problem::DeployedTypeChanged => {
                let (Some(mod_id), Some(sha)) = (f.mod_id, f.expect_sha256.as_ref()) else {
                    continue;
                };
                let src = staging_dirs
                    .get(&mod_id)
                    .map(|dir| deploy::rel_native(&ctx.paths.staging_dir.join(dir), &f.rel_path))
                    .transpose()?;
                let staged_ok = !broken_staged.contains(&(mod_id, f.path_key.as_str()))
                    && src.as_ref().is_some_and(|s| s.is_file());
                if !staged_ok {
                    actions.push(skip(f, "staged copy is also broken; reinstall the mod"));
                } else if f.problem == Problem::DeployedTypeChanged
                    && f.detail.as_deref() != Some("symlink")
                {
                    actions.push(skip(f, "a directory is in the way; remove it manually"));
                } else {
                    actions.push(RepairAction {
                        op: "recopy",
                        rel_path: f.rel_path.clone(),
                        mod_name: f.mod_name.clone(),
                        requires_force: f.problem != Problem::DeployedMissing,
                        note: Some(f.problem.describe().into()),
                        fix: Some(Fix::Recopy {
                            src: src.unwrap_or_default(),
                            target: deploy::rel_native(&root, &f.rel_path)?,
                            sha256: sha.clone(),
                            unlink: f.problem == Problem::DeployedTypeChanged,
                        }),
                    });
                }
            }
            Problem::StagedMissing | Problem::StagedModified => {
                let recoverable = deployed.get(&f.path_key).filter(|d| {
                    Some(d.provider_mod_id) == f.mod_id
                        && !broken_deployed.contains(f.path_key.as_str())
                });
                match (recoverable, f.mod_id, &f.expect_sha256) {
                    (Some(d), Some(mod_id), Some(sha)) => {
                        let dir = staging_dirs.get(&mod_id).ok_or_else(|| {
                            Error::NotFound(format!("mod {mod_id} for {}", f.rel_path))
                        })?;
                        actions.push(RepairAction {
                            op: "recover-staging",
                            rel_path: f.rel_path.clone(),
                            mod_name: f.mod_name.clone(),
                            requires_force: false,
                            note: Some(f.problem.describe().into()),
                            fix: Some(Fix::RecoverStaged {
                                from: deploy::rel_native(&root, &d.rel_path)?,
                                to: deploy::rel_native(
                                    &ctx.paths.staging_dir.join(dir),
                                    &f.rel_path,
                                )?,
                                sha256: sha.clone(),
                            }),
                        });
                    }
                    _ => actions.push(skip(f, "no intact copy exists; reinstall the mod")),
                }
            }
            Problem::BackupMissing | Problem::BackupModified => {
                actions.push(skip(
                    f,
                    "the original file is lost; nothing to restore from",
                ));
            }
            Problem::InterruptedDeployment | Problem::GameDirMissing => unreachable!(),
        }
    }
    Ok(RepairPlan {
        installation_id: inst.id,
        actions,
    })
}

fn skip(f: &Finding, note: &str) -> RepairAction {
    RepairAction {
        op: "skip",
        rel_path: f.rel_path.clone(),
        mod_name: f.mod_name.clone(),
        requires_force: false,
        note: Some(format!("{}; {note}", f.problem.describe())),
        fix: None,
    }
}

/// Apply a repair plan. Fixes that need `force` are skipped (and counted)
/// without it; each applied fix re-verifies the filesystem before acting.
pub fn execute_repair(
    ctx: &Context,
    inst: &Installation,
    plan: RepairPlan,
    force: bool,
) -> Result<RepairOutcome> {
    deploy::ensure_no_running(&ctx.db, inst.id)?;
    let root = deploy::target_root(inst)?;
    fs::create_dir_all(&root).path_ctx(&root)?;
    let canon_root = root.canonicalize().path_ctx(&root)?;

    let mut checked_dirs = HashSet::new();
    let mut out = RepairOutcome::default();
    for action in plan.actions {
        let Some(fix) = action.fix else {
            out.unrepairable += 1;
            continue;
        };
        if action.requires_force && !force {
            out.skipped_force += 1;
            continue;
        }
        match fix {
            Fix::Recopy {
                src,
                target,
                sha256,
                unlink,
            } => {
                if unlink {
                    match fs::symlink_metadata(&target) {
                        Ok(md) if md.file_type().is_symlink() => {
                            fs::remove_file(&target).path_ctx(&target)?;
                        }
                        Ok(md) if !md.is_file() => {
                            // A directory (or worse) appeared: not ours to delete.
                            out.unrepairable += 1;
                            continue;
                        }
                        _ => {}
                    }
                }
                deploy::check_containment(&canon_root, &target, true, &mut checked_dirs)?;
                deploy::place_file(ctx.config.deploy.method, &src, &target, &sha256)?;
                out.repaired += 1;
            }
            Fix::RecoverStaged { from, to, sha256 } => {
                // The deployed copy must still be intact right now.
                if !from.is_file() || sha256_file(&from)? != sha256 {
                    out.unrepairable += 1;
                    continue;
                }
                deploy::place_file(ctx.config.deploy.method, &from, &to, &sha256)?;
                out.repaired += 1;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Shared queries

struct DeployedRow {
    path_key: String,
    rel_path: String,
    provider_mod_id: i64,
    sha256: String,
}

fn deployed_rows(ctx: &Context, inst_id: i64) -> Result<Vec<DeployedRow>> {
    let mut stmt = ctx.db.conn.prepare(
        "SELECT path_key, rel_path, provider_mod_id, sha256
         FROM deployed_files WHERE installation_id = ?1 ORDER BY path_key",
    )?;
    let rows = stmt.query_map([inst_id], |r| {
        Ok(DeployedRow {
            path_key: r.get(0)?,
            rel_path: r.get(1)?,
            provider_mod_id: r.get(2)?,
            sha256: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn mod_names(ctx: &Context, inst_id: i64) -> Result<HashMap<i64, String>> {
    let mut stmt = ctx
        .db
        .conn
        .prepare("SELECT id, name FROM mods WHERE installation_id = ?1")?;
    let rows = stmt.query_map([inst_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn staging_dirs(ctx: &Context, inst_id: i64) -> Result<HashMap<i64, String>> {
    let mut stmt = ctx
        .db
        .conn
        .prepare("SELECT id, staging_dir FROM mods WHERE installation_id = ?1")?;
    let rows = stmt.query_map([inst_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
