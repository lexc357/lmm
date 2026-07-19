//! Stage 6 integration tests: deploy, purge, backups, rollback.

#![allow(clippy::unwrap_used)]

mod common;

use common::fixture;
use lmm_core::deploy::{self, PlanKind};
use lmm_core::error::Error;
use lmm_core::mods;

/// Snapshot of every regular file under the game dir (path -> content).
fn game_tree(fx: &common::Fixture) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in walkdir_files(&fx.game_dir) {
        let rel = entry
            .strip_prefix(&fx.game_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        files.push((rel, std::fs::read(&entry).unwrap()));
    }
    files.sort();
    files
}

fn walkdir_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn deploy(fx: &common::Fixture) -> deploy::Outcome {
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap()
}

fn deployed_count(fx: &common::Fixture) -> i64 {
    fx.ctx
        .db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM deployed_files WHERE installation_id = ?1",
            [fx.inst.id],
            |r| r.get(0),
        )
        .unwrap()
}

fn backups_count(fx: &common::Fixture) -> i64 {
    fx.ctx
        .db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM backups WHERE installation_id = ?1",
            [fx.inst.id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn deploy_backs_up_vanilla_and_purge_restores_pristine_tree() {
    let fx = fixture();
    let pristine = game_tree(&fx);

    let m = fx.install_mod(
        "overhaul",
        &[
            ("textures/new.dds", b"tex".as_slice()),
            ("Skyrim.esm", b"modded-esm".as_slice()),
        ],
    );
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    // Plan: vanilla Skyrim.esm gets backed up, the new texture does not.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert_eq!(plan.actions.len(), 2);
    assert!(!plan.requires_force());
    let esm = plan
        .actions
        .iter()
        .find(|a| a.rel_path == "Skyrim.esm")
        .unwrap();
    assert!(esm.backs_up_original);
    let tex = plan
        .actions
        .iter()
        .find(|a| a.rel_path == "textures/new.dds")
        .unwrap();
    assert!(!tex.backs_up_original);

    let outcome = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(outcome.installed, 2);
    assert_eq!(outcome.backed_up, 1);

    assert_eq!(fx.read_game_file("Data/Skyrim.esm").unwrap(), b"modded-esm");
    assert_eq!(fx.read_game_file("Data/textures/new.dds").unwrap(), b"tex");
    assert_eq!(deployed_count(&fx), 2);
    assert_eq!(backups_count(&fx), 1);

    // Second deploy: nothing to do.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert!(plan.is_empty());

    // Purge: back to the exact pre-lmm tree, all state cleared.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Purge).unwrap();
    let outcome = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(outcome.removed, 2);
    assert_eq!(outcome.restored, 1);
    assert_eq!(game_tree(&fx), pristine);
    assert_eq!(deployed_count(&fx), 0);
    assert_eq!(backups_count(&fx), 0);
}

#[test]
fn conflict_winner_follows_load_order_and_disable() {
    let fx = fixture();
    let a = fx.install_mod("mod-a", &[("textures/f.dds", b"AAA".as_slice())]);
    let b = fx.install_mod("mod-b", &[("textures/f.dds", b"BBB".as_slice())]);
    let profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, profile, &[a, b], true).unwrap();

    // b installed later -> higher priority -> wins.
    deploy(&fx);
    assert_eq!(fx.read_game_file("Data/textures/f.dds").unwrap(), b"BBB");

    // Move a to the end of the load order: a wins now.
    mods::set_position(&fx.ctx.db, profile, a, 2).unwrap();
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(plan.actions[0].op, "replace");
    deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(fx.read_game_file("Data/textures/f.dds").unwrap(), b"AAA");

    // Disable the winner: the next provider's file is restored.
    mods::set_enabled(&fx.ctx.db, profile, &[a], false).unwrap();
    deploy(&fx);
    assert_eq!(fx.read_game_file("Data/textures/f.dds").unwrap(), b"BBB");

    // Disable both: the file disappears and its empty dirs are pruned.
    mods::set_enabled(&fx.ctx.db, profile, &[b], false).unwrap();
    deploy(&fx);
    assert!(fx.read_game_file("Data/textures/f.dds").is_none());
    assert!(!fx.game_file("Data/textures").exists());
    assert_eq!(deployed_count(&fx), 0);
}

#[test]
fn external_modification_requires_force() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("scripts/x.pex", b"orig".as_slice())]);
    let profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, profile, &[m], true).unwrap();
    deploy(&fx);

    // Someone edits the deployed file behind lmm's back.
    std::fs::write(fx.game_file("Data/scripts/x.pex"), b"edited").unwrap();

    // Removing it must be refused without force...
    mods::set_enabled(&fx.ctx.db, profile, &[m], false).unwrap();
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert!(plan.requires_force());
    assert!(
        plan.actions[0]
            .warning
            .as_deref()
            .unwrap()
            .contains("modified")
    );
    let err = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap_err();
    assert!(matches!(err, Error::Blocked(_)), "{err}");
    assert_eq!(fx.read_game_file("Data/scripts/x.pex").unwrap(), b"edited");

    // ...and carried out with force.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    deploy::execute(&fx.ctx, &fx.inst, plan, true).unwrap();
    assert!(fx.read_game_file("Data/scripts/x.pex").is_none());
    assert_eq!(deployed_count(&fx), 0);
}

#[test]
fn replace_of_externally_modified_file_requires_force() {
    let fx = fixture();
    let a = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    let b = fx.install_mod("mod-b", &[("f.esp", b"BBB".as_slice())]);
    let profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, profile, &[a], true).unwrap();
    deploy(&fx);

    std::fs::write(fx.game_file("Data/f.esp"), b"edited").unwrap();
    mods::set_enabled(&fx.ctx.db, profile, &[b], true).unwrap();

    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert!(plan.requires_force());
    let err = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap_err();
    assert!(matches!(err, Error::Blocked(_)), "{err}");

    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    deploy::execute(&fx.ctx, &fx.inst, plan, true).unwrap();
    assert_eq!(fx.read_game_file("Data/f.esp").unwrap(), b"BBB");
}

#[test]
fn failed_write_rolls_back_to_pristine_tree() {
    let fx = fixture();
    let pristine = game_tree(&fx);

    // "Skyrim.esm" sorts before "zz.bsa": the esm (with its backup) is
    // written first, then the corrupted zz.bsa write fails.
    let m = fx.install_mod(
        "bad-mod",
        &[
            ("Skyrim.esm", b"modded-esm".as_slice()),
            ("zz.bsa", b"good-content".as_slice()),
        ],
    );
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    // Corrupt the staged copy of zz.bsa after install (hash no longer matches).
    let info = mods::get(&fx.ctx.db, m).unwrap();
    let staged = fx
        .ctx
        .paths
        .staging_dir
        .join(&info.staging_dir)
        .join("zz.bsa");
    std::fs::write(&staged, b"corrupted!!").unwrap();

    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    let err = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap_err();
    assert!(matches!(err, Error::Deploy(_)), "{err}");
    assert!(err.to_string().contains("rolled back"), "{err}");

    // Everything undone: vanilla esm back in place, no zz.bsa, no state.
    assert_eq!(game_tree(&fx), pristine);
    assert_eq!(deployed_count(&fx), 0);
    assert_eq!(backups_count(&fx), 0);
    let status: String = fx
        .ctx
        .db
        .conn
        .query_row(
            "SELECT status FROM deployments ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "rolled_back");
}

#[test]
fn missing_staged_file_blocks_plan() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("meshes/a.nif", b"nif".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    let info = mods::get(&fx.ctx.db, m).unwrap();
    std::fs::remove_file(
        fx.ctx
            .paths
            .staging_dir
            .join(&info.staging_dir)
            .join("meshes/a.nif"),
    )
    .unwrap();

    let err = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap_err();
    assert!(matches!(err, Error::Blocked(_)), "{err}");
    assert!(err.to_string().contains("staged file missing"), "{err}");
}

#[test]
fn externally_deleted_file_is_tolerated_on_remove() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("meshes/a.nif", b"nif".as_slice())]);
    let profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, profile, &[m], true).unwrap();
    deploy(&fx);

    std::fs::remove_file(fx.game_file("Data/meshes/a.nif")).unwrap();
    mods::set_enabled(&fx.ctx.db, profile, &[m], false).unwrap();

    // Already-gone files are drift, not danger: no force needed.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    assert!(!plan.requires_force());
    assert!(
        plan.actions[0]
            .warning
            .as_deref()
            .unwrap()
            .contains("already removed")
    );
    deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(deployed_count(&fx), 0);
}

#[test]
fn pending_deployment_blocks_and_rollback_recovers() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    // Simulate a crash: a deployment row stuck in 'running'.
    fx.ctx
        .db
        .conn
        .execute(
            "INSERT INTO deployments (installation_id, kind, status, started_at)
             VALUES (?1, 'deploy', 'running', 0)",
            [fx.inst.id],
        )
        .unwrap();

    let err = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap_err();
    assert!(matches!(err, Error::Blocked(_)), "{err}");
    assert!(err.to_string().contains("'rollback'"), "{err}");

    let rolled = deploy::rollback_running(&fx.ctx, &fx.inst).unwrap();
    assert!(rolled.is_some());
    assert!(
        deploy::find_running(&fx.ctx.db, fx.inst.id)
            .unwrap()
            .is_none()
    );

    // Normal operation resumes.
    deploy(&fx);
    assert_eq!(fx.read_game_file("Data/f.esp").unwrap(), b"AAA");
}

#[cfg(unix)]
fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

#[test]
#[cfg(unix)]
fn hardlink_method_links_deploys_and_purge_keeps_staging_intact() {
    use lmm_core::config::DeployMethod;

    let mut fx = fixture();
    fx.ctx.config.deploy.method = DeployMethod::Hardlink;
    let pristine = game_tree(&fx);

    let m = fx.install_mod(
        "linked-mod",
        &[
            ("textures/new.dds", b"tex".as_slice()),
            ("Skyrim.esm", b"modded-esm".as_slice()),
        ],
    );
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    let outcome = deploy(&fx);
    assert_eq!(outcome.installed, 2);
    assert_eq!(outcome.hardlinked, 2, "same filesystem: everything links");

    // The deployed file and its staged copy are one inode.
    let info = mods::get(&fx.ctx.db, m).unwrap();
    let staged = fx
        .ctx
        .paths
        .staging_dir
        .join(&info.staging_dir)
        .join("textures/new.dds");
    let deployed = fx.game_file("Data/textures/new.dds");
    assert_eq!(inode(&staged), inode(&deployed));

    // Purge unlinks the game copy; the staged copy survives untouched and
    // the vanilla tree comes back byte-for-byte.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Purge).unwrap();
    deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(game_tree(&fx), pristine);
    assert_eq!(std::fs::read(&staged).unwrap(), b"tex");
}

#[test]
fn copy_method_reports_no_hardlinks() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    let outcome = deploy(&fx);
    assert_eq!(outcome.installed, 1);
    assert_eq!(outcome.hardlinked, 0);
}

#[test]
#[cfg(unix)]
fn hardlink_deploy_detects_corrupt_staging_and_rolls_back() {
    use lmm_core::config::DeployMethod;

    let mut fx = fixture();
    fx.ctx.config.deploy.method = DeployMethod::Hardlink;
    let pristine = game_tree(&fx);

    let m = fx.install_mod("bad-mod", &[("zz.bsa", b"good-content".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();

    let info = mods::get(&fx.ctx.db, m).unwrap();
    let staged = fx
        .ctx
        .paths
        .staging_dir
        .join(&info.staging_dir)
        .join("zz.bsa");
    std::fs::write(&staged, b"corrupted!!").unwrap();

    // Linking a corrupt staged file must fail (never link unverified data),
    // and the failure rolls back like any other.
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    let err = deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap_err();
    assert!(matches!(err, Error::Deploy(_)), "{err}");
    assert_eq!(game_tree(&fx), pristine);
}

#[test]
fn profile_switch_redeploys_cleanly() {
    let fx = fixture();
    let a = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    let b = fx.install_mod("mod-b", &[("g.esp", b"BBB".as_slice())]);
    let default_profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, default_profile, &[a], true).unwrap();
    deploy(&fx);
    assert!(fx.read_game_file("Data/f.esp").is_some());

    // Second profile enables only mod-b; switching + deploying swaps files.
    let p2 = lmm_core::profile::create(&fx.ctx.db, fx.inst.id, "alt").unwrap();
    mods::set_enabled(&fx.ctx.db, p2.id, &[b], true).unwrap();
    lmm_core::profile::switch(&fx.ctx.db, fx.inst.id, "alt").unwrap();
    let inst = lmm_core::installs::get(&fx.ctx.db, fx.inst.id).unwrap();

    let plan = deploy::plan(&fx.ctx, &inst, PlanKind::Deploy).unwrap();
    deploy::execute(&fx.ctx, &inst, plan, false).unwrap();
    assert!(fx.read_game_file("Data/f.esp").is_none());
    assert_eq!(fx.read_game_file("Data/g.esp").unwrap(), b"BBB");
}
