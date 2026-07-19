use anyhow::{Result, bail};
use lmm_core::config::DeployMethod;
use lmm_core::deploy::{Outcome, Plan, PlanKind};
use lmm_core::{Context, deploy as core, installs};

use crate::output::{Out, print_table};

pub fn deploy(ctx: &Context, out: Out, game: Option<&str>, dry: bool, force: bool) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let plan = core::plan(ctx, &inst, PlanKind::Deploy)?;
    apply(ctx, out, &inst, plan, dry, force)
}

pub fn purge(ctx: &Context, out: Out, game: Option<&str>, dry: bool, force: bool) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let plan = core::plan(ctx, &inst, PlanKind::Purge)?;
    apply(ctx, out, &inst, plan, dry, force)
}

fn apply(
    ctx: &Context,
    out: Out,
    inst: &lmm_core::model::Installation,
    plan: Plan,
    dry: bool,
    force: bool,
) -> Result<()> {
    if dry {
        return out.emit(&plan, || print_plan(&plan));
    }
    if plan.is_empty() {
        return out.emit(&Outcome::default(), || {
            println!("nothing to do; the game directory matches the recorded state");
        });
    }
    if !out.json {
        print_plan(&plan);
    }
    if plan.requires_force() && !force {
        bail!(
            "some target files differ from what lmm recorded (see warnings above); \
             pass --force to override"
        );
    }
    let verb = match plan.kind {
        PlanKind::Deploy => format!("Apply {} change(s) to", plan.actions.len()),
        PlanKind::Purge => format!(
            "Remove all {} deployed file(s) and restore originals in",
            plan.actions.len()
        ),
    };
    if !out.confirm(&format!("{verb} {}?", inst.path.display()))? {
        bail!("aborted");
    }
    let outcome = core::execute(ctx, inst, plan, force)?;
    out.emit(&outcome, || {
        println!(
            "done: {} installed, {} replaced, {} removed ({} originals backed up, {} restored)",
            outcome.installed,
            outcome.replaced,
            outcome.removed,
            outcome.backed_up,
            outcome.restored
        );
        let written = outcome.installed + outcome.replaced;
        if ctx.config.deploy.method == DeployMethod::Hardlink && written > 0 {
            if outcome.hardlinked == written {
                println!("all {written} written file(s) hard-linked from staging");
            } else if outcome.hardlinked == 0 {
                println!(
                    "note: hard links unavailable (staging and the game are on \
                     different filesystems?); all {written} file(s) were copied"
                );
            } else {
                println!(
                    "{} of {written} written file(s) hard-linked; the rest were copied",
                    outcome.hardlinked
                );
            }
        }
    })
}

fn print_plan(plan: &Plan) {
    if plan.actions.is_empty() {
        println!("nothing to do; the game directory matches the recorded state");
        return;
    }
    let rows: Vec<Vec<String>> = plan
        .actions
        .iter()
        .map(|a| {
            let mut notes = Vec::new();
            if a.backs_up_original {
                notes.push("backs up original".to_string());
            }
            if a.restores_backup {
                notes.push("restores original".to_string());
            }
            if let Some(w) = &a.warning {
                notes.push(format!(
                    "warning: {w}{}",
                    if a.requires_force {
                        " (needs --force)"
                    } else {
                        ""
                    }
                ));
            }
            vec![
                a.op.to_string(),
                a.rel_path.clone(),
                a.mod_name.clone().unwrap_or_default(),
                notes.join("; "),
            ]
        })
        .collect();
    print_table(&["action", "file", "mod", "notes"], &rows);
    let count = |op: &str| plan.actions.iter().filter(|a| a.op == op).count();
    println!(
        "\n{} change(s): {} install, {} replace, {} remove",
        plan.actions.len(),
        count("install"),
        count("replace"),
        count("remove"),
    );
}

pub fn rollback(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let Some(pending) = core::find_running(&ctx.db, inst.id)? else {
        return out.emit(&serde_json::json!({ "rolled_back": null }), || {
            println!("no interrupted deployment to roll back");
        });
    };
    if !out.confirm(&format!(
        "Undo interrupted {} (id {}) on {}?",
        pending.kind,
        pending.id,
        inst.path.display()
    ))? {
        bail!("aborted");
    }
    let id = core::rollback_running(ctx, &inst)?;
    out.emit(&serde_json::json!({ "rolled_back": id }), || {
        println!("rolled back deployment {}", pending.id);
    })
}

pub fn verify(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let report = lmm_core::verify::report(ctx, &inst)?;
    out.emit(&report, || {
        if report.is_clean() {
            println!(
                "no drift: {} deployed, {} staged, {} backup file(s) all match the database",
                report.checked_deployed, report.checked_staged, report.checked_backups
            );
            return;
        }
        let rows: Vec<Vec<String>> = report
            .findings
            .iter()
            .map(|f| {
                vec![
                    f.problem.describe().to_string(),
                    f.rel_path.clone(),
                    f.mod_name.clone().unwrap_or_default(),
                    f.detail.clone().unwrap_or_default(),
                ]
            })
            .collect();
        print_table(&["problem", "file", "mod", "detail"], &rows);
        println!("\nrun 'repair --dry-run' to see what can be fixed");
    })?;
    if !report.is_clean() {
        bail!("{} problem(s) found", report.findings.len());
    }
    Ok(())
}

pub fn repair(ctx: &Context, out: Out, game: Option<&str>, dry: bool, force: bool) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let plan = lmm_core::verify::plan_repair(ctx, &inst)?;
    if dry {
        return out.emit(&plan, || print_repair_plan(&plan));
    }
    if plan.is_empty() {
        return out.emit(&lmm_core::verify::RepairOutcome::default(), || {
            println!("nothing to repair; no drift found");
        });
    }
    if !out.json {
        print_repair_plan(&plan);
    }
    if !out.confirm(&format!(
        "Repair {} item(s) on {}?",
        plan.actions.len(),
        inst.path.display()
    ))? {
        bail!("aborted");
    }
    let outcome = lmm_core::verify::execute_repair(ctx, &inst, plan, force)?;
    out.emit(&outcome, || {
        println!(
            "repaired {} file(s); {} skipped (need --force), {} unrepairable",
            outcome.repaired, outcome.skipped_force, outcome.unrepairable
        );
    })
}

fn print_repair_plan(plan: &lmm_core::verify::RepairPlan) {
    if plan.actions.is_empty() {
        println!("nothing to repair; no drift found");
        return;
    }
    let rows: Vec<Vec<String>> = plan
        .actions
        .iter()
        .map(|a| {
            let mut notes = a.note.clone().unwrap_or_default();
            if a.requires_force {
                notes.push_str(" (needs --force)");
            }
            vec![
                a.op.to_string(),
                a.rel_path.clone(),
                a.mod_name.clone().unwrap_or_default(),
                notes,
            ]
        })
        .collect();
    print_table(&["action", "file", "mod", "notes"], &rows);
}
