use std::path::Path;

use anyhow::{Result, bail};
use lmm_core::error::Error as CoreError;
use lmm_core::{Context, fomod, installs, mods, resolve};

use crate::output::{Out, print_table};

pub fn install(
    ctx: &Context,
    out: Out,
    game: Option<&str>,
    archive: &Path,
    name: Option<String>,
    version: Option<String>,
    manual: bool,
) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    out.verbose(format!("installing into [{}] {}", inst.id, inst.game_name));
    let prepared = mods::prepare(
        ctx,
        &inst,
        archive,
        &mods::InstallOptions {
            name: name.as_deref(),
            version: version.as_deref(),
        },
    )?;

    // FOMOD detection: the standard `install` runs installers
    // automatically; --manual skips them.
    if !manual && let Some(detected) = fomod::detect(prepared.extracted.root())? {
        match super::fomod::install_flow(ctx, out, &inst, &prepared, &detected) {
            Ok(Some(installed)) => return report_installed(out, &installed),
            Ok(None) => return Ok(()), // cancelled; already reported
            // Broken installer metadata: offer the plain-archive fallback
            // (safe — it goes through normal layout detection and the user
            // decides). Anything else propagates.
            Err(e)
                if matches!(
                    e.downcast_ref::<CoreError>(),
                    Some(CoreError::Fomod(_) | CoreError::FomodUnsupported(_))
                ) =>
            {
                out.info(format!("FOMOD installer problem: {e:#}"));
                if !out.confirm("Install the archive as-is instead (skip the installer)?")? {
                    bail!("aborted");
                }
            }
            Err(e) => return Err(e),
        }
    }

    let installed = mods::finish_plain(ctx, &inst, &prepared)?;
    report_installed(out, &installed)
}

fn report_installed(out: Out, installed: &mods::Installed) -> Result<()> {
    out.emit(&installed.info, || {
        println!(
            "installed '{}' ({} files) — layout: {}",
            installed.info.name, installed.info.file_count, installed.layout_rule
        );
        if installed.layout_uncertain {
            println!(
                "warning: archive layout was not recognized; check 'deploy --dry-run' before deploying"
            );
        }
        println!("enable it with: lmm enable '{}'", installed.info.name);
    })
}

pub fn list(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let list = mods::list_for_profile(&ctx.db, profile_id)?;
    let conflicts = resolve::conflicts(&ctx.db, profile_id)?;

    out.emit(&list, || {
        if list.is_empty() {
            println!("no mods installed; add one with 'install <archive>'");
            return;
        }
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|m| {
                vec![
                    m.priority.to_string(),
                    m.info.id.to_string(),
                    if m.enabled { "on".into() } else { "off".into() },
                    m.info.name.clone(),
                    m.info.version.clone().unwrap_or_default(),
                    m.info.file_count.to_string(),
                ]
            })
            .collect();
        print_table(&["order", "id", "state", "name", "version", "files"], &rows);
        if !conflicts.is_empty() {
            println!(
                "\n{} conflicting paths between enabled mods; see 'conflicts'",
                conflicts.len()
            );
        }
    })
}

pub fn set_enabled(
    ctx: &Context,
    out: Out,
    game: Option<&str>,
    selectors: &[String],
    enabled: bool,
) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let mut ids = Vec::new();
    let mut names = Vec::new();
    for sel in selectors {
        let m = mods::find(&ctx.db, inst.id, sel)?;
        ids.push(m.id);
        names.push(m.name);
    }
    mods::set_enabled(&ctx.db, profile_id, &ids, enabled)?;
    out.emit(
        &serde_json::json!({ "mods": names, "enabled": enabled }),
        || {
            println!(
                "{}: {} (apply with 'deploy')",
                if enabled { "enabled" } else { "disabled" },
                names.join(", ")
            );
        },
    )
}

pub fn order(ctx: &Context, out: Out, game: Option<&str>, selector: &str, pos: i64) -> Result<()> {
    if pos < 1 {
        bail!("position is 1-based");
    }
    let inst = installs::select(&ctx.db, game)?;
    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let m = mods::find(&ctx.db, inst.id, selector)?;
    mods::set_position(&ctx.db, profile_id, m.id, pos)?;
    list(ctx, out, game)
}

pub fn uninstall(ctx: &Context, out: Out, game: Option<&str>, selector: &str) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let m = mods::find(&ctx.db, inst.id, selector)?;
    if !out.confirm(&format!(
        "Uninstall '{}' ({} files) from all profiles?",
        m.name, m.file_count
    ))? {
        bail!("aborted");
    }
    mods::uninstall(ctx, &inst, m.id)?;
    out.emit(&serde_json::json!({ "uninstalled": m.name }), || {
        println!("uninstalled '{}'", m.name);
    })
}

pub fn conflicts(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let conflicts = resolve::conflicts(&ctx.db, profile_id)?;
    out.emit(&conflicts, || {
        if conflicts.is_empty() {
            println!("no conflicts between enabled mods");
            return;
        }
        for c in &conflicts {
            println!("{}", c.path_key);
            for (i, p) in c.providers.iter().enumerate() {
                println!(
                    "  {} {} (order {})",
                    if i == 0 { "wins:  " } else { "loses: " },
                    p.mod_name,
                    p.priority
                );
            }
        }
        println!(
            "\n{} conflicting paths; later load order (higher number) wins — change with 'order'",
            conflicts.len()
        );
    })
}
