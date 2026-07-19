use anyhow::{Result, bail};
use lmm_core::discovery::steam;
use lmm_core::{Context, installs};

use crate::args::GameCmd;
use crate::output::{Out, fmt_time, print_table};

pub fn scan(ctx: &Context, out: Out, all: bool) -> Result<()> {
    let apps = steam::discover(&ctx.config)?;
    let supported: Vec<&steam::SteamApp> = apps.iter().filter(|a| a.game_slug.is_some()).collect();
    let shown: Vec<&steam::SteamApp> = if all {
        apps.iter().collect()
    } else {
        supported.clone()
    };

    out.emit(&shown, || {
        if apps.is_empty() {
            println!("no Steam games found (no Steam roots with steamapps/)");
            return;
        }
        if shown.is_empty() {
            println!(
                "found {} Steam apps but none are supported games (rerun with --all to list them)",
                apps.len()
            );
            return;
        }
        let registered = installs::list(&ctx.db).unwrap_or_default();
        let rows: Vec<Vec<String>> = shown
            .iter()
            .map(|a| {
                let is_registered = registered
                    .iter()
                    .any(|i| i.path == a.install_dir);
                vec![
                    a.app_id.to_string(),
                    a.game_slug.clone().unwrap_or_else(|| "-".into()),
                    a.name.clone(),
                    if a.proton_prefix.is_some() { "yes".into() } else { "-".into() },
                    if is_registered { "yes".into() } else { "-".into() },
                    a.install_dir.display().to_string(),
                ]
            })
            .collect();
        print_table(&["app", "game", "name", "proton", "added", "path"], &rows);
        if !all {
            println!(
                "\n{} supported of {} Steam apps found; register one with 'lmm game add --app <app>'",
                supported.len(),
                apps.len()
            );
        }
    })
}

pub fn launch(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    if let Some(d) = lmm_core::deploy::find_running(&ctx.db, inst.id)? {
        bail!(
            "an interrupted {} is pending; run 'lmm rollback' before launching",
            d.kind
        );
    }
    let method = lmm_core::launch::method(&inst)?;
    let lmm_core::launch::LaunchMethod::SteamUrl { url } = &method;
    // Fire-and-forget: Steam takes over from here.
    let spawned = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        bail!("could not run xdg-open ({e}); open {url} yourself to launch the game");
    }
    out.emit(&method, || {
        println!("launching {} via {url}", inst.game_name);
    })
}

pub fn game(ctx: &Context, out: Out, cmd: GameCmd) -> Result<()> {
    match cmd {
        GameCmd::Add {
            path,
            app,
            slug,
            label,
        } => add(ctx, out, path, app, slug, label),
        GameCmd::List => list(ctx, out),
        GameCmd::Use { install } => {
            let inst = installs::find(&ctx.db, &install)?;
            installs::set_default(&ctx.db, inst.id)?;
            out.emit(&inst, || {
                println!(
                    "default installation: [{}] {} at {}",
                    inst.id,
                    inst.game_name,
                    inst.path.display()
                );
            })
        }
        GameCmd::Remove { install } => {
            let inst = installs::find(&ctx.db, &install)?;
            if !out.confirm(&format!(
                "Unregister [{}] {} ({})? Installed mods and their staging files will be deleted.",
                inst.id,
                inst.game_name,
                inst.path.display()
            ))? {
                bail!("aborted");
            }
            installs::remove(&ctx.db, inst.id)?;
            out.emit(&serde_json::json!({ "removed": inst.id }), || {
                println!("removed installation {}", inst.id);
            })
        }
    }
}

fn add(
    ctx: &Context,
    out: Out,
    path: Option<std::path::PathBuf>,
    app: Option<u32>,
    slug: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let inst = if let Some(app_id) = app {
        // Register straight from discovery: path, prefix and game type all
        // come from Steam's own metadata.
        let apps = steam::discover(&ctx.config)?;
        let Some(found) = apps.iter().find(|a| a.app_id == app_id) else {
            bail!("Steam app {app_id} not found by scan; is the game installed?");
        };
        let slug = match slug.or_else(|| found.game_slug.clone()) {
            Some(s) => s,
            None => bail!(
                "app {} ({}) is not a supported game; pass --slug generic to manage it anyway",
                app_id,
                found.name
            ),
        };
        installs::add(
            &ctx.db,
            &installs::NewInstallation {
                game_slug: &slug,
                path: &found.install_dir,
                source: "steam",
                steam_library: Some(&found.library),
                proton_prefix: found.proton_prefix.as_deref(),
                label: label.as_deref(),
            },
        )?
    } else {
        let Some(path) = path else {
            bail!("either a game path or --app <id> is required");
        };
        let slug = slug.unwrap_or_else(|| "generic".to_string());
        installs::add(
            &ctx.db,
            &installs::NewInstallation {
                game_slug: &slug,
                path: &path,
                source: "manual",
                steam_library: None,
                proton_prefix: None,
                label: label.as_deref(),
            },
        )?
    };
    out.emit(&inst, || {
        println!(
            "registered [{}] {} at {}",
            inst.id,
            inst.game_name,
            inst.path.display()
        );
    })
}

fn list(ctx: &Context, out: Out) -> Result<()> {
    let insts = installs::list(&ctx.db)?;
    let default = ctx.db.setting("default_installation")?;
    out.emit(&insts, || {
        if insts.is_empty() {
            println!("no installations registered; run 'lmm scan' then 'lmm game add'");
            return;
        }
        let rows: Vec<Vec<String>> = insts
            .iter()
            .map(|i| {
                let is_default = default.as_deref() == Some(&i.id.to_string());
                vec![
                    format!("{}{}", i.id, if is_default { "*" } else { "" }),
                    i.game_slug.clone(),
                    i.label.clone().unwrap_or_default(),
                    i.source.clone(),
                    i.path.display().to_string(),
                    fmt_time(i.created_at),
                ]
            })
            .collect();
        print_table(&["id", "game", "label", "source", "path", "added"], &rows);
        if insts.len() > 1 && default.is_none() {
            println!("\nhint: set a default with 'lmm game use <id>'");
        }
    })
}
