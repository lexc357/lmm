use anyhow::Result;
use lmm_core::model::Deployment;
use lmm_core::{Context, deploy, installs, mods, profile, resolve};
use serde::Serialize;

use crate::output::{Out, fmt_time};

#[derive(Serialize)]
struct Status {
    data_dir: String,
    db_path: String,
    installations: usize,
    current: Option<lmm_core::model::Installation>,
    profile: Option<String>,
    mods_installed: usize,
    mods_enabled: usize,
    conflicts: usize,
    deployed_files: i64,
    backups: i64,
    last_deployment: Option<Deployment>,
    pending_deployment: bool,
}

pub fn status(ctx: &Context, out: Out, game: Option<&str>) -> Result<()> {
    let all = installs::list(&ctx.db)?;
    // Status must work with zero or many installations, so a failed selection
    // is informational here, not an error.
    let current = installs::select(&ctx.db, game).ok();

    let mut st = Status {
        data_dir: ctx.paths.data_dir.display().to_string(),
        db_path: ctx.paths.db_path.display().to_string(),
        installations: all.len(),
        current,
        profile: None,
        mods_installed: 0,
        mods_enabled: 0,
        conflicts: 0,
        deployed_files: 0,
        backups: 0,
        last_deployment: None,
        pending_deployment: false,
    };
    if let Some(inst) = &st.current {
        let profile_id = mods::active_profile_id(ctx, inst)?;
        st.profile = profile::list(&ctx.db, inst.id)?
            .into_iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.name);
        let list = mods::list_for_profile(&ctx.db, profile_id)?;
        st.mods_installed = list.len();
        st.mods_enabled = list.iter().filter(|m| m.enabled).count();
        st.conflicts = resolve::conflicts(&ctx.db, profile_id)?.len();
        st.deployed_files = count(ctx, "deployed_files", inst.id)?;
        st.backups = count(ctx, "backups", inst.id)?;
        st.last_deployment = deploy::last(&ctx.db, inst.id)?;
        st.pending_deployment = deploy::find_running(&ctx.db, inst.id)?.is_some();
    }

    out.emit(&st, || {
        println!("data dir:       {}", st.data_dir);
        println!("database:       {}", st.db_path);
        println!("installations:  {}", st.installations);
        let Some(i) = &st.current else {
            if st.installations == 0 {
                println!("\nno games registered; run 'lmm scan' to find Steam games");
            } else {
                println!("\nmultiple installations; select one with --game or 'lmm game use'");
            }
            return;
        };
        println!(
            "current game:   [{}] {} ({})",
            i.id,
            i.game_name,
            i.path.display()
        );
        println!(
            "active profile: {}",
            st.profile.as_deref().unwrap_or("default")
        );
        println!(
            "mods:           {} installed, {} enabled{}",
            st.mods_installed,
            st.mods_enabled,
            if st.conflicts > 0 {
                format!(" ({} conflicting paths)", st.conflicts)
            } else {
                String::new()
            }
        );
        println!(
            "deployed:       {} file(s), {} original(s) backed up",
            st.deployed_files, st.backups
        );
        match &st.last_deployment {
            Some(d) => println!(
                "last run:       {} {} at {}",
                d.kind,
                d.status,
                fmt_time(d.finished_at.unwrap_or(d.started_at))
            ),
            None => println!("last run:       never deployed"),
        }
        if st.pending_deployment {
            println!("\nwarning: an interrupted deployment is pending; run 'lmm rollback'");
        }
    })
}

fn count(ctx: &Context, table: &str, inst_id: i64) -> Result<i64> {
    // `table` is a compile-time constant at every call site, never user input.
    Ok(ctx.db.conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE installation_id = ?1"),
        [inst_id],
        |r| r.get(0),
    )?)
}
