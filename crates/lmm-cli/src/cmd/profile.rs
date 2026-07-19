use anyhow::{Result, bail};
use lmm_core::{Context, installs, mods, profile};

use crate::args::ProfileCmd;
use crate::output::{Out, fmt_time, print_table};

pub fn profile(ctx: &Context, out: Out, game: Option<&str>, cmd: ProfileCmd) -> Result<()> {
    let inst = installs::select(&ctx.db, game)?;
    // Ensure the active-profile pointer exists before any profile operation.
    mods::active_profile_id(ctx, &inst)?;

    match cmd {
        ProfileCmd::List => {
            let profiles = profile::list(&ctx.db, inst.id)?;
            out.emit(&profiles, || {
                let rows: Vec<Vec<String>> = profiles
                    .iter()
                    .map(|p| {
                        vec![
                            format!("{}{}", p.name, if p.is_active { "*" } else { "" }),
                            fmt_time(p.created_at),
                        ]
                    })
                    .collect();
                print_table(&["profile", "created"], &rows);
            })
        }
        ProfileCmd::Create { name } => {
            let p = profile::create(&ctx.db, inst.id, &name)?;
            out.emit(&p, || {
                println!("created profile '{}' (all mods disabled)", p.name);
            })
        }
        ProfileCmd::Switch { name } => {
            let p = profile::switch(&ctx.db, inst.id, &name)?;
            out.emit(&p, || {
                println!(
                    "active profile: '{}' — run 'deploy' to apply its mod set",
                    p.name
                );
            })
        }
        ProfileCmd::Delete { name } => {
            if !out.confirm(&format!("Delete profile '{name}'?"))? {
                bail!("aborted");
            }
            profile::delete(&ctx.db, inst.id, &name)?;
            out.emit(&serde_json::json!({ "deleted": name }), || {
                println!("deleted profile '{name}'");
            })
        }
        ProfileCmd::Copy { from, to } => {
            let p = profile::copy(&ctx.db, inst.id, &from, &to)?;
            out.emit(&p, || {
                println!("copied profile '{from}' to '{}'", p.name);
            })
        }
    }
}
