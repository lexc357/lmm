//! The terminal FOMOD installer and the `fomod` command family.
//!
//! This file is the *frontend* only: rendering, prompting, and
//! orchestration. All installer logic (selection rules, conditions, plan
//! building, persistence) lives in `lmm_core::fomod`; nothing here decides
//! which files get installed.
//!
//! Interaction model, per step:
//!
//! ```text
//! Step 1 of 3: Texture Resolution
//!
//! Resolution (select exactly one)
//!   1. ( ) Low
//!   2. (*) Medium
//!   3. ( ) High        [recommended]
//!   4. (x) Ultra       — requires flag 'enb' = 'on'
//!
//! Choice [Enter=next] (numbers, all/none, d N=details, back, cancel, help):
//! ```
//!
//! Multi-group steps number choices as `group.option` (e.g. `2.1`).
//! Reading happens on plain stdin between shell prompts, exactly like
//! `Out::confirm`, so the flow works identically inside the interactive
//! shell and as a one-shot command. Non-interactive runs (`--yes` or piped
//! stdin with `--yes`) accept the installer's defaults and print them.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use lmm_core::fomod::{self, cond, plan, session, store};
use lmm_core::games::Layout;
use lmm_core::hash::sha256_file;
use lmm_core::model::Installation;
use lmm_core::{Context, installs, mods, staging};

use crate::args::FomodCmd;
use crate::output::{Out, print_table};

pub fn fomod_cmd(ctx: &Context, out: Out, game: Option<&str>, cmd: FomodCmd) -> Result<()> {
    match cmd {
        FomodCmd::Inspect { target } => inspect(ctx, out, game, &target),
        FomodCmd::Choices { r#mod } => choices(ctx, out, game, &r#mod),
        FomodCmd::Validate { archive } => validate(ctx, out, game, &archive),
        FomodCmd::Reinstall { r#mod, archive } => {
            reinstall(ctx, out, game, &r#mod, archive.as_deref())
        }
        FomodCmd::Reconfigure { r#mod, archive } => {
            reconfigure(ctx, out, game, &r#mod, archive.as_deref())
        }
    }
}

/// Destination fix-ups for an installation's game.
fn dest_rules(inst: &Installation) -> plan::DestRules {
    let strip = lmm_core::games::by_slug(&inst.game_slug)
        .map(|g| g.layout == Layout::BethesdaData)
        .unwrap_or(false);
    plan::DestRules {
        strip_data_prefix: strip,
    }
}

// ---------------------------------------------------------------------------
// install flow (called from cmd::mods::install on detection)

/// Run the interactive FOMOD flow for a prepared install and finish it.
/// Returns Ok(None) if the user cancelled.
pub fn install_flow(
    ctx: &Context,
    out: Out,
    inst: &Installation,
    prepared: &mods::Prepared,
    detected: &fomod::Detected,
) -> Result<Option<mods::Installed>> {
    let (module, info) = fomod::load(detected)?;
    out.info(format!("\nFOMOD installer detected: {}", module.name));
    if let (Some(author), Some(version)) = (&info.author, &info.version) {
        out.info(format!("by {author}, version {version}"));
    }
    for w in &module.warnings {
        out.verbose(format!("installer warning: {w}"));
    }

    let profile_id = mods::active_profile_id(ctx, inst)?;
    let game = prepared.game;
    let env = fomod::env::InstallEnvironment::new(&ctx.db, inst, game, profile_id);

    // Module-level preconditions: failing or unknown needs a human call.
    if let Some(deps) = &module.module_dependencies {
        match cond::eval_composite(deps, &cond::Flags::new(), &env) {
            cond::Eval::True => {}
            cond::Eval::False | cond::Eval::Unknown(_) => {
                out.info("this mod declares requirements that are not (verifiably) met:");
                for line in cond::explain_composite(deps, &cond::Flags::new(), &env) {
                    out.info(format!("  - {line}"));
                }
                if !out.confirm("Continue installing anyway?")? {
                    out.info("installation cancelled");
                    return Ok(None);
                }
            }
        }
    }

    let mut sess = session::Session::new(&module, &env);
    let selections = match drive_session(ctx, out, &mut sess, detected) {
        Ok(Some(sel)) => sel,
        Ok(None) => {
            out.info("installation cancelled");
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    let plan_files = match build_plan_interactive(
        out,
        &module,
        &selections,
        detected.installer_root.as_path(),
        &env,
        dest_rules(inst),
    )? {
        Some(p) => p,
        None => {
            out.info("installation cancelled");
            return Ok(None);
        }
    };

    let config_sha256 = sha256_file(&detected.config_path)?;
    let installed = mods::finish_fomod(
        ctx,
        inst,
        prepared,
        &detected.installer_root,
        &mods::FomodData {
            module_name: &module.name,
            config_sha256: &config_sha256,
            selections: &selections,
            plan: &plan_files,
        },
    )?;
    Ok(Some(installed))
}

/// Run the step loop. Ok(None) = user cancelled.
fn drive_session(
    ctx: &Context,
    out: Out,
    sess: &mut session::Session<'_>,
    detected: &fomod::Detected,
) -> Result<Option<session::Selections>> {
    // Non-interactive: accept defaults, but show what was decided.
    if out.yes || !std::io::stdin().is_terminal() {
        if !out.yes {
            bail!(
                "this archive uses a FOMOD installer, which needs a terminal \
                 (or --yes to accept its defaults, or --manual to skip it)"
            );
        }
        let selections = sess.finish().map_err(|e| {
            anyhow!("{e}\nthe installer's defaults are not a valid selection; run interactively")
        })?;
        print_selections(out, &selections);
        return Ok(Some(selections));
    }

    if sess.module().steps.is_empty() {
        // Nothing to ask; requiredInstallFiles-only installer.
        return Ok(Some(sess.finish()?));
    }

    loop {
        let view = sess.current()?;
        let (pos, total) = sess.position();
        render_step(out, &view, pos, total);
        match prompt_step(ctx, out, sess, &view, detected)? {
            StepOutcome::Next => match sess.advance() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => out.info(format!("cannot continue: {e}")),
            },
            StepOutcome::Back => {
                if !sess.back() {
                    out.info("already at the first step");
                }
            }
            StepOutcome::Cancel => return Ok(None),
        }
    }
    Ok(Some(sess.finish()?))
}

enum StepOutcome {
    Next,
    Back,
    Cancel,
}

fn render_step(out: Out, view: &session::StepView, pos: usize, total: usize) {
    out.info(format!("\nStep {pos} of {total}: {}", view.name));
    if let Some(note) = &view.visibility_note {
        out.info(format!("note: {note}"));
    }
    let multi = view.groups.len() > 1;
    for group in &view.groups {
        out.info(format!("\n{} ({})", group.name, group.rule.describe()));
        for opt in &group.options {
            let number = if multi {
                format!("{}.{}", group.index + 1, opt.index + 1)
            } else {
                format!("{}", opt.index + 1)
            };
            let mark = if opt.selected {
                "(*)"
            } else if opt.locked.is_some() {
                "(x)"
            } else {
                "( )"
            };
            let mut line = format!("  {number}. {mark} {}", opt.name);
            if let Some(reason) = &opt.locked {
                line.push_str(&format!("  — {reason}"));
            } else if let Some(note) = &opt.note {
                line.push_str(&format!("  [{note}]"));
            }
            out.info(line);
        }
    }
}

/// One prompt/execute round per input line until the step changes.
fn prompt_step(
    ctx: &Context,
    out: Out,
    sess: &mut session::Session<'_>,
    first_view: &session::StepView,
    detected: &fomod::Detected,
) -> Result<StepOutcome> {
    let mut multi = first_view.groups.len() > 1;
    loop {
        eprint!("\nChoice [Enter=next] (numbers, all/none, d N=details, back, cancel, help): ");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(StepOutcome::Cancel); // EOF
        }
        let line = line.trim().to_string();
        match line.as_str() {
            "" | "next" | "n" => return Ok(StepOutcome::Next),
            "back" | "b" => return Ok(StepOutcome::Back),
            "cancel" | "quit" | "q" => return Ok(StepOutcome::Cancel),
            "help" | "h" | "?" => {
                out.info(HELP);
                continue;
            }
            _ => {}
        }

        let result = (|| -> Result<()> {
            if let Some(rest) = line.strip_prefix("all") {
                let g = parse_group_arg(rest, multi)?;
                sess.select_all(g).map_err(Into::into)
            } else if let Some(rest) = line.strip_prefix("none") {
                let g = parse_group_arg(rest, multi)?;
                sess.select_none(g).map_err(Into::into)
            } else if let Some(rest) = line.strip_prefix("d ").or(line.strip_prefix("open ")) {
                let (g, o) = parse_ref(rest.trim(), multi)?;
                show_details(ctx, out, sess, g, o, line.starts_with("open "), detected)
            } else {
                // Comma-separated selection toggles: "1,3" or "2.1,2.3".
                for part in line.split(',') {
                    let (g, o) = parse_ref(part.trim(), multi)?;
                    toggle(sess, g, o)?;
                }
                Ok(())
            }
        })();
        if let Err(e) = result {
            out.info(format!("  {e:#}"));
        }

        let view = sess.current()?;
        multi = view.groups.len() > 1;
        let (pos, total) = sess.position();
        render_step(out, &view, pos, total);
    }
}

const HELP: &str = "\
  3          select option 3 (single-group steps)
  2.3        select option 3 of group 2
  1,3,4      several at once; selecting a selected option deselects it
  all / none whole group (add a group number on multi-group steps)
  d N        show an option's description and image path
  open N     also open its image with your configured viewer ([fomod].image_viewer)
  Enter/next validate this step and continue     back  previous step
  cancel     abort the installation (nothing has been written)";

/// "2.3" -> (1, 2); "3" -> (0, 2) when the step has a single group.
fn parse_ref(s: &str, multi: bool) -> Result<(usize, usize)> {
    let parse = |x: &str| -> Result<usize> {
        let n: usize = x
            .trim()
            .parse()
            .map_err(|_| anyhow!("'{s}' is not a number (type 'help' for syntax)"))?;
        if n == 0 {
            bail!("numbers start at 1");
        }
        Ok(n - 1)
    };
    match s.split_once('.') {
        Some((g, o)) => Ok((parse(g)?, parse(o)?)),
        None if multi => bail!("this step has several groups; use group.option (e.g. 2.1)"),
        None => Ok((0, parse(s)?)),
    }
}

/// "" -> group 0; " 2" -> group 1.
fn parse_group_arg(rest: &str, multi: bool) -> Result<usize> {
    let rest = rest.trim();
    if rest.is_empty() {
        if multi {
            bail!("this step has several groups; say e.g. 'all 2'");
        }
        return Ok(0);
    }
    let n: usize = rest
        .parse()
        .map_err(|_| anyhow!("'{rest}' is not a group number"))?;
    if n == 0 {
        bail!("group numbers start at 1");
    }
    Ok(n - 1)
}

fn toggle(sess: &mut session::Session<'_>, g: usize, o: usize) -> Result<()> {
    let view = sess.current()?;
    let group = view
        .groups
        .get(g)
        .ok_or_else(|| anyhow!("no group {}", g + 1))?;
    let opt = group
        .options
        .get(o)
        .ok_or_else(|| anyhow!("group '{}' has no option {}", group.name, o + 1))?;
    if opt.selected {
        sess.deselect(g, o)?;
    } else {
        sess.select(g, o)?;
    }
    Ok(())
}

fn show_details(
    ctx: &Context,
    out: Out,
    sess: &mut session::Session<'_>,
    g: usize,
    o: usize,
    open_image: bool,
    detected: &fomod::Detected,
) -> Result<()> {
    let view = sess.current()?;
    let group = view
        .groups
        .get(g)
        .ok_or_else(|| anyhow!("no group {}", g + 1))?;
    let opt = group
        .options
        .get(o)
        .ok_or_else(|| anyhow!("group '{}' has no option {}", group.name, o + 1))?;
    out.info(format!("\n--- {} ---", opt.name));
    out.info(if opt.description.is_empty() {
        "(no description)"
    } else {
        &opt.description
    });
    if let Some(note) = &opt.note {
        out.info(format!("note: {note}"));
    }
    if let Some(reason) = &opt.locked {
        out.info(format!("locked: {reason}"));
    }
    match &opt.image {
        None => {}
        Some(raw) => match lmm_core::paths::RelPath::parse(raw) {
            Err(e) => out.info(format!("image: (invalid path in installer: {e})")),
            Ok(rel) => {
                let path = rel.to_native(&detected.installer_root);
                if path.is_file() {
                    out.info(format!("image: {}", path.display()));
                    if open_image {
                        open_with_viewer(ctx, out, &path);
                    }
                } else {
                    out.info(format!("image: {raw} (not present in the archive)"));
                }
            }
        },
    }
    Ok(())
}

/// Spawn the *user-configured* viewer on an image path. Nothing from the
/// archive is ever executed; without configuration this only prints.
fn open_with_viewer(ctx: &Context, out: Out, path: &Path) {
    match &ctx.config.fomod.image_viewer {
        None => out.info("no [fomod].image_viewer configured; showing the path only"),
        Some(viewer) => match std::process::Command::new(viewer).arg(path).spawn() {
            Ok(_) => {}
            Err(e) => out.info(format!("could not start '{viewer}': {e}")),
        },
    }
}

/// Build the plan, interactively resolving unknown conditional installs
/// and confirming ambiguities. Ok(None) = cancelled.
fn build_plan_interactive(
    out: Out,
    module: &fomod::Module,
    selections: &session::Selections,
    installer_root: &Path,
    env: &dyn cond::Environment,
    rules: plan::DestRules,
) -> Result<Option<Vec<plan::PlannedFile>>> {
    let mut resolutions: BTreeMap<usize, bool> = BTreeMap::new();
    let outcome = loop {
        let outcome = plan::build(module, selections, installer_root, env, rules, &resolutions)?;
        if outcome.unresolved.is_empty() {
            break outcome;
        }
        for u in &outcome.unresolved {
            out.info(format!(
                "\nThis installer wants to install {} file(s) when: {}\n\
                 That condition cannot be checked here: {}",
                u.file_count, u.condition, u.reason
            ));
            let include = out.confirm("Install these files anyway?")?;
            resolutions.insert(u.index, include);
        }
    };

    let plan::InstallPlan {
        files,
        ambiguities,
        notes,
    } = outcome.plan;
    for n in &notes {
        out.verbose(format!("plan: {n}"));
    }
    if !ambiguities.is_empty() {
        out.info("\nSelected options overlap and the installer does not define a winner:");
        for a in &ambiguities {
            out.info(format!(
                "  {}\n    installing from: {}\n    ignoring:        {}",
                a.dest, a.winner, a.loser
            ));
        }
        if !out.confirm("Proceed with these files?")? {
            return Ok(None);
        }
    }
    out.info(format!("\n{} file(s) will be installed", files.len()));
    Ok(Some(files))
}

fn print_selections(out: Out, selections: &session::Selections) {
    for step in &selections.steps {
        for group in &step.groups {
            for opt in &group.options {
                out.info(format!(
                    "  {} > {} > {}",
                    step.step, group.group, opt.option
                ));
            }
        }
    }
    if !selections.flags.is_empty() {
        let flags: Vec<String> = selections
            .flags
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        out.verbose(format!("flags: {}", flags.join(", ")));
    }
}

// ---------------------------------------------------------------------------
// fomod inspect / choices / validate

fn inspect(ctx: &Context, out: Out, game: Option<&str>, target: &str) -> Result<()> {
    let path = Path::new(target);
    if path.exists() {
        return inspect_archive(ctx, out, path);
    }
    // Not a file: treat as an installed-mod selector.
    choices(ctx, out, game, target)
}

fn inspect_archive(ctx: &Context, out: Out, archive: &Path) -> Result<()> {
    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, archive)?;
    let Some(detected) = fomod::detect(extracted.root())? else {
        bail!(
            "{}: no FOMOD installer found (fomod/ModuleConfig.xml missing)",
            archive.display()
        );
    };
    let (module, info) = fomod::load(&detected)?;
    out.emit(&module, || {
        println!("module:  {}", module.name);
        if let Some(a) = &info.author {
            println!("author:  {a}");
        }
        if let Some(v) = &info.version {
            println!("version: {v}");
        }
        if let Some(d) = &module.module_dependencies {
            println!("requires: {}", cond::describe_composite(d));
        }
        println!(
            "required files: {} mapping(s); conditional patterns: {}",
            module.required_files.len(),
            module.conditional_installs.len()
        );
        for (si, step) in module.steps.iter().enumerate() {
            print!("step {}: {}", si + 1, step.name);
            match &step.visible {
                Some(v) => println!("  [visible when: {}]", cond::describe_composite(v)),
                None => println!(),
            }
            for group in &step.groups {
                println!("  group: {} ({})", group.name, group.rule.describe());
                for opt in &group.options {
                    let kind = match &opt.type_desc {
                        fomod::model::TypeDescriptor::Simple(t) => format!("{t:?}"),
                        fomod::model::TypeDescriptor::Dependent { default, .. } => {
                            format!("{default:?} (condition-dependent)")
                        }
                    };
                    println!(
                        "    - {} [{kind}] ({} mapping(s))",
                        opt.name,
                        opt.files.len()
                    );
                }
            }
        }
        for w in &module.warnings {
            println!("warning: {w}");
        }
    })
}

fn choices(ctx: &Context, out: Out, game: Option<&str>, selector: &str) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let m = mods::find(&ctx.db, inst.id, selector)?;
    let Some(record) = store::get(&ctx.db, m.id)? else {
        bail!("'{}' was not installed through a FOMOD installer", m.name);
    };
    out.emit(
        &serde_json::json!({
            "mod": m.name,
            "module": record.module_name,
            "config_sha256": record.config_sha256,
            "archive_sha256": m.archive_sha256,
            "format": record.format,
            "choices": record.selections,
            "plan_files": record.plan.len(),
        }),
        || {
            println!("mod:     {}", m.name);
            println!("module:  {}", record.module_name);
            println!("format:  {}", record.format);
            println!("installer config sha256: {}", record.config_sha256);
            println!("archive sha256:          {}", m.archive_sha256);
            if record.selections.steps.is_empty() {
                println!("choices: none (installer had no options)");
            } else {
                println!("choices:");
                print_selections(Out { json: false, ..out }, &record.selections);
            }
            if !record.selections.flags.is_empty() {
                println!("flags:");
                for (k, v) in &record.selections.flags {
                    println!("  {k} = {v}");
                }
            }
            println!(
                "plan: {} file(s); see 'fomod inspect' on the archive for details",
                record.plan.len()
            );
        },
    )
}

fn validate(ctx: &Context, out: Out, game: Option<&str>, archive: &Path) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, archive)?;
    let Some(detected) = fomod::detect(extracted.root())? else {
        bail!(
            "{}: no FOMOD installer found (fomod/ModuleConfig.xml missing)",
            archive.display()
        );
    };
    let (module, _info) = fomod::load(&detected)?;
    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let game_def = lmm_core::games::by_slug(&inst.game_slug)
        .ok_or_else(|| anyhow!("unknown game slug '{}'", inst.game_slug))?;
    let env = fomod::env::InstallEnvironment::new(&ctx.db, &inst, game_def, profile_id);
    let report = plan::validate(&module, &detected.installer_root, &env, dest_rules(&inst))?;

    let fatal = report.fatal();
    out.emit(&report, || {
        println!(
            "installer detected: {} (fomod/ModuleConfig.xml)",
            report.module_name
        );
        println!(
            "info.xml: {}",
            if detected.info_path.is_some() {
                "present"
            } else {
                "absent"
            }
        );
        let rows = vec![
            vec!["steps".to_string(), report.steps.to_string()],
            vec!["groups".to_string(), report.groups.to_string()],
            vec!["options".to_string(), report.options.to_string()],
            vec!["file mappings".to_string(), report.mappings.to_string()],
        ];
        print_table(&["structure", "count"], &rows);
        let section = |title: &str, items: &[String]| {
            if !items.is_empty() {
                println!("\n{title}:");
                for i in items {
                    println!("  - {i}");
                }
            }
        };
        section("missing sources (fatal)", &report.missing_sources);
        section("invalid paths (fatal)", &report.invalid_paths);
        section(
            "conditions not evaluable on this machine",
            &report.unsupported_conditions,
        );
        section("missing images", &report.missing_images);
        section("warnings", &report.warnings);
        if !fatal {
            println!("\nno fatal problems found");
        }
    })?;
    if fatal {
        bail!("validation found fatal problems");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// reinstall / reconfigure

/// Find the archive for a mod: an explicit override, or the download store
/// by hash. The returned path is verified readable; hash agreement with
/// `expect_sha` is reported to the caller.
fn locate_archive(
    ctx: &Context,
    m: &lmm_core::model::Mod,
    override_path: Option<&Path>,
) -> Result<(PathBuf, bool)> {
    if let Some(p) = override_path {
        let sha = sha256_file(p)?;
        return Ok((p.to_path_buf(), sha == m.archive_sha256));
    }
    if let Some(p) = store::find_archive_by_sha(&ctx.db, &m.archive_sha256)? {
        let path = PathBuf::from(p);
        if path.is_file() && sha256_file(&path)? == m.archive_sha256 {
            return Ok((path, true));
        }
    }
    bail!(
        "cannot find the original archive for '{}' (hash {}…); pass --archive <path>",
        m.name,
        &m.archive_sha256[..12]
    );
}

fn reinstall(
    ctx: &Context,
    out: Out,
    game: Option<&str>,
    selector: &str,
    archive: Option<&Path>,
) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let m = mods::find(&ctx.db, inst.id, selector)?;
    let Some(record) = store::get(&ctx.db, m.id)? else {
        bail!("'{}' was not installed through a FOMOD installer", m.name);
    };
    let (archive_path, hash_matches) = locate_archive(ctx, &m, archive)?;
    if !hash_matches {
        bail!(
            "{} differs from the archive '{}' was installed from; \
             a saved plan can only be replayed against identical bytes — use 'fomod reconfigure'",
            archive_path.display(),
            m.name
        );
    }

    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, &archive_path)?;
    let Some(detected) = fomod::detect(extracted.root())? else {
        bail!("the archive no longer contains a FOMOD installer");
    };
    // Identical archive bytes imply an identical ModuleConfig; verify anyway
    // (defense against a tampered download store).
    let config_sha256 = sha256_file(&detected.config_path)?;
    if config_sha256 != record.config_sha256 {
        bail!("installer configuration hash changed; use 'fomod reconfigure'");
    }

    if !out.confirm(&format!(
        "Reinstall '{}' by replaying its saved plan ({} files)?",
        m.name,
        record.plan.len()
    ))? {
        bail!("aborted");
    }
    let installed = mods::replace_fomod_install(
        ctx,
        &inst,
        m.id,
        &extracted,
        &detected.installer_root,
        &mods::FomodData {
            module_name: &record.module_name,
            config_sha256: &record.config_sha256,
            selections: &record.selections,
            plan: &record.plan,
        },
    )?;
    out.emit(&installed.info, || {
        println!(
            "reinstalled '{}' ({} files)",
            installed.info.name, installed.info.file_count
        );
    })
}

fn reconfigure(
    ctx: &Context,
    out: Out,
    game: Option<&str>,
    selector: &str,
    archive: Option<&Path>,
) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    let m = mods::find(&ctx.db, inst.id, selector)?;
    let Some(record) = store::get(&ctx.db, m.id)? else {
        bail!("'{}' was not installed through a FOMOD installer", m.name);
    };
    let (archive_path, hash_matches) = locate_archive(ctx, &m, archive)?;
    if !hash_matches {
        out.info(format!(
            "note: {} differs from the originally installed archive (upgrade?); \
             saved choices apply only where they still fit",
            archive_path.display()
        ));
    }

    let extracted = staging::extract_archive(&ctx.paths, &ctx.config.limits, &archive_path)?;
    let Some(detected) = fomod::detect(extracted.root())? else {
        bail!("the archive does not contain a FOMOD installer");
    };
    let (module, _info) = fomod::load(&detected)?;
    let config_sha256 = sha256_file(&detected.config_path)?;
    if config_sha256 != record.config_sha256 {
        out.info(
            "note: the installer configuration changed since these choices were saved; \
             they are preselected only where they still resolve",
        );
    }

    let profile_id = mods::active_profile_id(ctx, &inst)?;
    let game_def = lmm_core::games::by_slug(&inst.game_slug)
        .ok_or_else(|| anyhow!("unknown game slug '{}'", inst.game_slug))?;
    let env = fomod::env::InstallEnvironment::new(&ctx.db, &inst, game_def, profile_id);

    out.info(format!("\nReconfiguring: {}", module.name));
    let (mut sess, notes) = session::Session::with_preselected(&module, &env, &record.selections);
    for n in &notes {
        out.info(format!("note: {n}"));
    }
    let selections = match drive_session(ctx, out, &mut sess, &detected)? {
        Some(sel) => sel,
        None => {
            out.info("reconfiguration cancelled; the installed mod is unchanged");
            return Ok(());
        }
    };
    let plan_files = match build_plan_interactive(
        out,
        &module,
        &selections,
        &detected.installer_root,
        &env,
        dest_rules(&inst),
    )? {
        Some(p) => p,
        None => {
            out.info("reconfiguration cancelled; the installed mod is unchanged");
            return Ok(());
        }
    };

    // Show what will change compared to the current installation.
    show_plan_diff(out, &record.plan, &plan_files);
    if !out.confirm(&format!("Apply these changes to '{}'?", m.name))? {
        out.info("reconfiguration cancelled; the installed mod is unchanged");
        return Ok(());
    }

    let installed = mods::replace_fomod_install(
        ctx,
        &inst,
        m.id,
        &extracted,
        &detected.installer_root,
        &mods::FomodData {
            module_name: &module.name,
            config_sha256: &config_sha256,
            selections: &selections,
            plan: &plan_files,
        },
    )?;
    out.emit(&installed.info, || {
        println!(
            "reconfigured '{}' ({} files); deploy to apply",
            installed.info.name, installed.info.file_count
        );
    })
}

/// added / removed / replaced summary between two plans, by destination.
fn show_plan_diff(out: Out, old: &[plan::PlannedFile], new: &[plan::PlannedFile]) {
    let old_map: BTreeMap<String, &plan::PlannedFile> =
        old.iter().map(|f| (f.dest.key(), f)).collect();
    let new_map: BTreeMap<String, &plan::PlannedFile> =
        new.iter().map(|f| (f.dest.key(), f)).collect();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut replaced = 0usize;
    for (key, nf) in &new_map {
        match old_map.get(key) {
            None => {
                added += 1;
                out.info(format!("  + {}  ({})", nf.dest, nf.origin));
            }
            Some(of) if of.source != nf.source => {
                replaced += 1;
                out.info(format!("  ~ {}  (now from {})", nf.dest, nf.origin));
            }
            Some(_) => {}
        }
    }
    for (key, of) in &old_map {
        if !new_map.contains_key(key) {
            removed += 1;
            out.info(format!("  - {}", of.dest));
        }
    }
    out.info(format!(
        "\nplan changes: {added} added, {removed} removed, {replaced} replaced, {} unchanged",
        new_map.len() - added - replaced
    ));
}
