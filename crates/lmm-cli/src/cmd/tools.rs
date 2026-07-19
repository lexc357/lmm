//! The Game Tools section: tool status/install/launch, guided setup,
//! game configuration, load-order maintenance and the health check.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use lmm_core::model::Installation;
use lmm_core::tools::registry::ToolDef;
use lmm_core::tools::{
    self, ToolLaunch, ToolState, gameconfig, health, install as tool_install, loadorder,
};
use lmm_core::{Context, installs};

use crate::args::{LoadorderCmd, ToolsCmd, ToolsConfigCmd};
use crate::output::{Out, print_table};

pub fn tools(ctx: &Context, out: Out, game_sel: Option<&str>, cmd: Option<ToolsCmd>) -> Result<()> {
    let inst = installs::select(&ctx.db, game_sel)?;
    match cmd.unwrap_or(ToolsCmd::List) {
        ToolsCmd::List => list(ctx, out, &inst),
        ToolsCmd::Install {
            tool,
            archive,
            version,
            force,
        } => install(ctx, out, &inst, &tool, archive.as_deref(), version, force),
        ToolsCmd::Verify { tool } => verify(ctx, out, &inst, tool.as_deref()),
        ToolsCmd::Launch { tool } => launch(ctx, out, &inst, &tool),
        ToolsCmd::Remove { tool, force } => remove(ctx, out, &inst, &tool, force),
        ToolsCmd::Setup => setup(ctx, out, &inst),
        ToolsCmd::Check => check(ctx, out, &inst),
        ToolsCmd::Config { cmd } => config(ctx, out, &inst, cmd.unwrap_or(ToolsConfigCmd::Show)),
        ToolsCmd::Loadorder { cmd } => match cmd.unwrap_or(LoadorderCmd::Analyze) {
            LoadorderCmd::Analyze => lo_analyze(ctx, out, &inst),
            LoadorderCmd::Sort { dry_run } => lo_sort(ctx, out, &inst, dry_run),
            LoadorderCmd::Backups => lo_backups(ctx, out, &inst),
            LoadorderCmd::Restore { backup } => lo_restore(ctx, out, &inst, backup.as_deref()),
        },
    }
}

fn list(ctx: &Context, out: Out, inst: &Installation) -> Result<()> {
    let statuses = tools::status(ctx, inst)?;
    out.emit(&statuses, || {
        println!("Game Tools for {}:\n", inst.game_name);
        let rows: Vec<Vec<String>> = statuses
            .iter()
            .map(|s| {
                let notes = match (&s.detail, s.state) {
                    (Some(d), _) => d.clone(),
                    (None, ToolState::Missing) => format!("download: {}", s.url),
                    _ => String::new(),
                };
                vec![
                    s.id.clone(),
                    s.name.clone(),
                    s.state.describe().to_string(),
                    s.tier.describe().to_string(),
                    s.version.clone().unwrap_or_default(),
                    notes,
                ]
            })
            .collect();
        print_table(
            &["tool", "name", "status", "tier", "version", "notes"],
            &rows,
        );
        println!(
            "\ninstall/update from a downloaded archive: 'tools install <tool> <archive>'\n\
             first-time setup: 'tools setup' — readiness check: 'tools check'"
        );
    })
}

fn install(
    ctx: &Context,
    out: Out,
    inst: &Installation,
    tool_sel: &str,
    archive: Option<&Path>,
    version: Option<String>,
    force: bool,
) -> Result<()> {
    let tool = tools::find_tool(inst, tool_sel)?;
    let Some(archive) = archive else {
        return where_to_get(out, tool);
    };
    let installed = tool_install::install(ctx, inst, tool, archive, version.as_deref(), force)?;
    out.emit(&installed, || {
        println!(
            "installed {}{} — {} file(s) into {}",
            tool.name,
            installed
                .version
                .as_deref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default(),
            installed.files,
            installed.target_root.display()
        );
        if installed.backed_up > 0 {
            println!(
                "{} existing file(s) were backed up and will be restored on 'tools remove'",
                installed.backed_up
            );
        }
        if installed.stale_removed > 0 {
            println!(
                "{} file(s) from the previous version were removed",
                installed.stale_removed
            );
        }
    })
}

/// `tools install <tool>` without an archive: explain how to obtain it.
fn where_to_get(out: Out, tool: &ToolDef) -> Result<()> {
    out.emit(
        &serde_json::json!({
            "tool": tool.id, "url": tool.url,
            "nexus": tool.nexus.map(|(d, id)| serde_json::json!({"domain": d, "mod_id": id})),
        }),
        || {
            println!("{} — {}", tool.name, tool.summary);
            println!("download: {}", tool.url);
            if tool.nexus.is_some() {
                println!(
                    "on Nexus Mods you can click \"Mod Manager Download\" (with 'nexus register' \
                     set up) or download manually,"
                );
            }
            println!(
                "then install the archive with: tools install {} <path-to-archive>",
                tool.id
            );
        },
    )
}

fn verify(ctx: &Context, out: Out, inst: &Installation, tool_sel: Option<&str>) -> Result<()> {
    let ids = match tool_sel {
        Some(sel) => vec![tools::find_tool(inst, sel)?.id.to_string()],
        None => {
            let ids = tool_install::managed_ids(&ctx.db, inst.id)?;
            if ids.is_empty() {
                bail!("no tools are managed by lmm for this installation");
            }
            ids
        }
    };
    let mut report: Vec<(String, Vec<tool_install::VerifiedFile>)> = Vec::new();
    for id in &ids {
        let tool = tools::find_tool(inst, id)?;
        report.push((id.clone(), tool_install::verify(ctx, inst, tool)?));
    }
    out.emit(
        &report
            .iter()
            .map(|(id, files)| serde_json::json!({ "tool": id, "files": files }))
            .collect::<Vec<_>>(),
        || {
            let mut clean = true;
            for (id, files) in &report {
                let bad: Vec<_> = files
                    .iter()
                    .filter(|f| f.state != tool_install::FileState::Ok)
                    .collect();
                if bad.is_empty() {
                    println!("{id}: {} file(s) ok", files.len());
                } else {
                    clean = false;
                    println!(
                        "{id}: {} of {} file(s) not as recorded:",
                        bad.len(),
                        files.len()
                    );
                    for f in bad {
                        println!(
                            "  {} {}",
                            match f.state {
                                tool_install::FileState::Missing => "missing ",
                                _ => "modified",
                            },
                            f.rel_path
                        );
                    }
                }
            }
            if !clean {
                println!(
                    "\nreinstall the affected tool to repair ('tools install <tool> <archive>')"
                );
            }
        },
    )
}

fn launch(ctx: &Context, out: Out, inst: &Installation, tool_sel: &str) -> Result<()> {
    let tool = tools::find_tool(inst, tool_sel)?;
    let method = tools::launch_method(ctx, inst, tool)?;
    let mut command = match &method {
        ToolLaunch::Native { exe, cwd } => {
            let mut c = std::process::Command::new(exe);
            c.current_dir(cwd);
            c
        }
        ToolLaunch::Proton {
            proton,
            exe,
            cwd,
            compat_data,
            steam_root,
        } => {
            let mut c = std::process::Command::new(proton);
            c.arg("run")
                .arg(exe)
                .current_dir(cwd)
                .env("STEAM_COMPAT_DATA_PATH", compat_data)
                .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam_root);
            c
        }
    };
    // Fire-and-forget, like game launch: the tool outlives this invocation.
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start {}: {e}", tool.name))?;
    out.emit(&method, || match &method {
        ToolLaunch::Native { exe, .. } => println!("launched {} ({})", tool.name, exe.display()),
        ToolLaunch::Proton { exe, .. } => {
            println!("launched {} through Proton ({})", tool.name, exe.display());
        }
    })
}

fn remove(ctx: &Context, out: Out, inst: &Installation, tool_sel: &str, force: bool) -> Result<()> {
    let tool = tools::find_tool(inst, tool_sel)?;
    if !out.confirm(&format!(
        "Remove {} and restore any files it displaced?",
        tool.name
    ))? {
        bail!("aborted");
    }
    let removed = tool_install::remove(ctx, inst, tool, force)?;
    out.emit(&removed, || {
        println!(
            "removed {}: {} file(s) deleted, {} original(s) restored",
            tool.name, removed.removed, removed.restored
        );
        for s in &removed.skipped {
            println!("  kept {s}: modified outside lmm (--force removes it)");
        }
    })
}

fn check(ctx: &Context, out: Out, inst: &Installation) -> Result<()> {
    let game = tools::catalog(inst)?;
    let checks = health::run(ctx, inst, game)?;
    out.emit(&checks, || {
        println!("Modding setup check for {}:\n", inst.game_name);
        for c in &checks {
            let mark = match c.status {
                health::CheckStatus::Ok => "ok  ",
                health::CheckStatus::Warn => "WARN",
                health::CheckStatus::Fail => "FAIL",
                health::CheckStatus::Skip => "skip",
            };
            println!("[{mark}] {:<28} {}", c.name, c.detail);
            if let Some(r) = &c.recommendation {
                println!("       -> {r}");
            }
        }
        let fails = checks
            .iter()
            .filter(|c| c.status == health::CheckStatus::Fail)
            .count();
        let warns = checks
            .iter()
            .filter(|c| c.status == health::CheckStatus::Warn)
            .count();
        println!();
        if fails == 0 && warns == 0 {
            println!("ready for modding");
        } else {
            println!("{fails} problem(s), {warns} warning(s)");
        }
    })
}

fn config(ctx: &Context, out: Out, inst: &Installation, cmd: ToolsConfigCmd) -> Result<()> {
    let game = tools::catalog(inst)?;
    if game.tweaks.is_empty() {
        bail!(
            "'{}' needs no configuration changes for modding",
            inst.game_slug
        );
    }
    match cmd {
        ToolsConfigCmd::Show => {
            let tweaks = gameconfig::status(inst, game)?;
            out.emit(&tweaks, || {
                let rows: Vec<Vec<String>> = tweaks
                    .iter()
                    .map(|t| {
                        let state = match &t.state {
                            gameconfig::TweakState::Applied => "applied".to_string(),
                            gameconfig::TweakState::NotApplied { current: Some(v) } => {
                                format!("not applied (currently '{v}')")
                            }
                            gameconfig::TweakState::NotApplied { current: None } => {
                                "not applied (setting absent)".into()
                            }
                            gameconfig::TweakState::FileMissing => "file missing".into(),
                        };
                        vec![
                            t.id.clone(),
                            format!("{} [{}] {}={}", t.file, t.section, t.key, t.value),
                            state,
                        ]
                    })
                    .collect();
                print_table(&["setting", "change", "state"], &rows);
                println!("\nwhy each change is needed:");
                for t in &tweaks {
                    println!("  {}: {}", t.id, t.why);
                }
                println!("\napply with 'tools config apply' (originals are backed up)");
            })
        }
        ToolsConfigCmd::Apply => {
            if !out.confirm(&format!(
                "Apply the recommended modding settings for {}? Files are backed up first.",
                inst.game_name
            ))? {
                bail!("aborted");
            }
            let applied = gameconfig::apply(ctx, inst, game)?;
            out.emit(&applied, || {
                if applied.applied.is_empty() {
                    println!("all settings were already applied; nothing changed");
                } else {
                    println!("applied: {}", applied.applied.join(", "));
                }
                if !applied.backed_up.is_empty() {
                    println!(
                        "backed up original(s): {} (undo anytime with 'tools config restore')",
                        applied.backed_up.join(", ")
                    );
                }
            })
        }
        ToolsConfigCmd::Restore => {
            if !out.confirm(&format!(
                "Restore {}'s configuration files to their pre-lmm state?",
                inst.game_name
            ))? {
                bail!("aborted");
            }
            let restored = gameconfig::restore(ctx, inst, game)?;
            out.emit(&restored, || {
                for f in &restored.restored {
                    println!("restored {f}");
                }
                for f in &restored.deleted {
                    println!("deleted {f} (lmm had created it)");
                }
            })
        }
    }
}

fn lo_analyze(ctx: &Context, out: Out, inst: &Installation) -> Result<()> {
    let _ = ctx; // analysis is filesystem-only
    let game = tools::catalog(inst)?;
    let a = loadorder::analyze(inst, game)?;
    out.emit(&a, || {
        print_analysis(&a);
        if !a.issues.is_empty() {
            println!("\nfix ordering issues with 'tools loadorder sort'");
        }
    })
}

fn print_analysis(a: &loadorder::Analysis) {
    if a.plugins.is_empty() {
        println!(
            "no plugins listed ({}{})",
            a.path.display(),
            if a.path.exists() {
                ""
            } else {
                " does not exist"
            }
        );
    } else {
        println!("load order ({}):", a.path.display());
        for (i, p) in a.plugins.iter().enumerate() {
            println!(
                "{:>3}  {}{}{}",
                i + 1,
                if p.enabled { "" } else { "(disabled) " },
                p.name,
                if p.present { "" } else { "  [file missing]" },
            );
        }
    }
    if !a.unlisted.is_empty() {
        println!("\nin Data but not listed (never loaded):");
        for n in &a.unlisted {
            println!("     {n}");
        }
    }
    if !a.issues.is_empty() {
        println!("\nissues:");
        for i in &a.issues {
            println!("  {}: {}", i.plugin, i.detail);
        }
    }
    if a.timestamp_caveat {
        println!(
            "\nnote: this game also orders plugins by file time; if problems persist \
             after sorting, run LOOT (see 'tools')"
        );
    }
}

fn lo_sort(ctx: &Context, out: Out, inst: &Installation, dry_run: bool) -> Result<()> {
    let game = tools::catalog(inst)?;
    let a = loadorder::analyze(inst, game)?;
    if a.plugins.is_empty() {
        bail!("nothing to sort: no plugins listed in {}", a.path.display());
    }
    let plan = loadorder::plan_sort(&a, game);
    if !plan.changed {
        return out.emit(&plan, || {
            println!("load order already follows best practices; nothing to do");
        });
    }
    if dry_run {
        return out.emit(&plan, || {
            println!("would reorder plugins as follows (dry run):");
            for (i, name) in plan.after.iter().enumerate() {
                let moved = plan.before.get(i) != Some(name);
                println!("{:>3}  {}{}", i + 1, name, if moved { "  *" } else { "" });
            }
        });
    }
    if !out.confirm("Sort the plugin load order? The current order is backed up first.")? {
        bail!("aborted");
    }
    let backup = loadorder::apply_sort(ctx, inst, game, &a, &plan)?;
    out.emit(
        &serde_json::json!({ "plan": plan, "backup": backup }),
        || {
            println!(
                "sorted {} plugin(s); previous order backed up",
                plan.after.len()
            );
            println!("undo anytime with 'tools loadorder restore'");
        },
    )
}

fn lo_backups(ctx: &Context, out: Out, inst: &Installation) -> Result<()> {
    let backups = loadorder::backups(ctx, inst)?;
    out.emit(&backups, || {
        if backups.is_empty() {
            println!("no load-order backups yet (one is taken before every sort/restore)");
            return;
        }
        for b in &backups {
            println!("{}", b.display());
        }
    })
}

fn lo_restore(ctx: &Context, out: Out, inst: &Installation, backup: Option<&Path>) -> Result<()> {
    let game = tools::catalog(inst)?;
    if !out.confirm(
        "Replace the current plugin order with the backup? (The current order is backed up first.)",
    )? {
        bail!("aborted");
    }
    let used = loadorder::restore(ctx, inst, game, backup)?;
    out.emit(&serde_json::json!({ "restored_from": used }), || {
        println!("load order restored from {}", used.display());
    })
}

// ---------------------------------------------------------------------------
// Guided first-time setup.

fn setup(ctx: &Context, out: Out, inst: &Installation) -> Result<()> {
    if out.json {
        bail!("'tools setup' is interactive; script the individual commands instead");
    }
    let game = tools::catalog(inst)?;
    println!("Recommended modding setup for {}\n", inst.game_name);

    // 1. Tools: walk essential + recommended, offering each missing one.
    let statuses = tools::status(ctx, inst)?;
    let mut skipped: Vec<&str> = Vec::new();
    for st in &statuses {
        if st.tier == lmm_core::tools::registry::Tier::Optional {
            continue;
        }
        let tool = tools::find_tool(inst, &st.id)?;
        match st.state {
            ToolState::Installed => {
                println!("[ok] {} — installed", st.name);
                continue;
            }
            ToolState::Outdated => {
                println!(
                    "[!!] {} — {}",
                    st.name,
                    st.detail.as_deref().unwrap_or("outdated")
                );
            }
            ToolState::Attention => {
                println!(
                    "[!!] {} — {}",
                    st.name,
                    st.detail.as_deref().unwrap_or("needs attention")
                );
            }
            ToolState::Missing => println!(
                "[--] {} ({}) — {}",
                st.name,
                st.tier.describe(),
                st.summary_line()
            ),
        }
        println!("     download: {}", st.url);
        match prompt_archive(out, &st.name)? {
            Some(path) => match tool_install::install(ctx, inst, tool, &path, None, false) {
                Ok(installed) => println!(
                    "     installed {} file(s) into {}",
                    installed.files,
                    installed.target_root.display()
                ),
                Err(e) => println!("     install failed: {e} — continuing setup"),
            },
            None => skipped.push(tool.id),
        }
    }

    // 2. Game configuration.
    if !game.tweaks.is_empty() {
        println!();
        match gameconfig::status(inst, game) {
            Ok(tweaks) => {
                let missing = tweaks
                    .iter()
                    .filter(|t| t.state != gameconfig::TweakState::Applied)
                    .count();
                if missing == 0 {
                    println!("[ok] game configuration — all settings applied");
                } else if out
                    .confirm(&format!(
                        "{missing} modding setting(s) are not applied; apply them now? \
                         (originals are backed up)"
                    ))
                    .unwrap_or(false)
                {
                    let applied = gameconfig::apply(ctx, inst, game)?;
                    println!("     applied: {}", applied.applied.join(", "));
                } else {
                    println!("     skipped ('tools config apply' does this later)");
                }
            }
            Err(e) => println!("[--] game configuration not checkable: {e}"),
        }
    }

    // 3. Load order.
    if game.plugins.is_some() {
        println!();
        match loadorder::analyze(inst, game) {
            Ok(a) if !a.plugins.is_empty() => {
                let plan = loadorder::plan_sort(&a, game);
                if a.issues.is_empty() && !plan.changed {
                    println!("[ok] plugin load order — no issues");
                } else if out
                    .confirm(&format!(
                        "load order: {} issue(s) found; sort plugins now? (current order is backed up)",
                        a.issues.len()
                    ))
                    .unwrap_or(false)
                {
                    loadorder::apply_sort(ctx, inst, game, &a, &plan)?;
                    println!("     sorted {} plugin(s)", plan.after.len());
                } else {
                    println!("     skipped ('tools loadorder sort' does this later)");
                }
            }
            Ok(_) => println!("[--] no plugins listed yet; sort after installing mods"),
            Err(e) => println!("[--] load order not checkable: {e}"),
        }
    }

    // 4. Where things stand now.
    println!();
    if !skipped.is_empty() {
        println!(
            "skipped tools: {} — install anytime with 'tools install <tool> <archive>'",
            skipped.join(", ")
        );
    }
    println!("setup finished; verify with 'tools check', then install mods with 'install'");
    Ok(())
}

/// Ask for a downloaded archive path for a tool; empty input (or a
/// non-interactive session) skips the tool.
fn prompt_archive(out: Out, name: &str) -> Result<Option<PathBuf>> {
    if out.yes || !std::io::stdin().is_terminal() {
        println!("     (non-interactive: skipping install)");
        return Ok(None);
    }
    eprint!("     path to downloaded {name} archive (Enter to skip): ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    // Tolerate dragged-in quotes and ~.
    let line = line.trim_matches(['\'', '"']);
    let path = if let Some(rest) = line.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(line),
        }
    } else {
        PathBuf::from(line)
    };
    if !path.exists() {
        println!("     {} not found; skipping", path.display());
        return Ok(None);
    }
    Ok(Some(path))
}

/// Small display helper on ToolStatus for setup output.
trait SummaryLine {
    fn summary_line(&self) -> String;
}

impl SummaryLine for lmm_core::tools::ToolStatus {
    fn summary_line(&self) -> String {
        self.detail
            .clone()
            .unwrap_or_else(|| "not installed".into())
    }
}
