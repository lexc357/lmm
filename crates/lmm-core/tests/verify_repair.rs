//! Stage 7 integration tests: verify (drift report) and repair.

#![allow(clippy::unwrap_used)]

mod common;

use common::fixture;
use lmm_core::deploy::{self, PlanKind};
use lmm_core::error::Error;
use lmm_core::mods;
use lmm_core::verify::{self, Problem};

fn deploy(fx: &common::Fixture) {
    let plan = deploy::plan(&fx.ctx, &fx.inst, PlanKind::Deploy).unwrap();
    deploy::execute(&fx.ctx, &fx.inst, plan, false).unwrap();
}

fn staged_path(fx: &common::Fixture, mod_id: i64, rel: &str) -> std::path::PathBuf {
    let info = mods::get(&fx.ctx.db, mod_id).unwrap();
    fx.ctx.paths.staging_dir.join(&info.staging_dir).join(rel)
}

fn problems(report: &verify::Report) -> Vec<Problem> {
    report.findings.iter().map(|f| f.problem).collect()
}

#[test]
fn clean_state_verifies_clean() {
    let fx = fixture();
    let m = fx.install_mod(
        "mod-a",
        &[
            ("f.esp", b"AAA".as_slice()),
            ("Skyrim.esm", b"modded".as_slice()),
        ],
    );
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.checked_deployed, 2);
    assert_eq!(report.checked_staged, 2);
    assert_eq!(report.checked_backups, 1);
}

#[test]
fn detects_and_repairs_missing_and_modified_deployed_files() {
    let fx = fixture();
    let m = fx.install_mod(
        "mod-a",
        &[
            ("meshes/a.nif", b"AAA".as_slice()),
            ("meshes/b.nif", b"BBB".as_slice()),
        ],
    );
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    std::fs::remove_file(fx.game_file("Data/meshes/a.nif")).unwrap();
    std::fs::write(fx.game_file("Data/meshes/b.nif"), b"edited").unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    let mut found = problems(&report);
    found.sort_by_key(|p| format!("{p:?}"));
    assert_eq!(
        found,
        vec![Problem::DeployedMissing, Problem::DeployedModified]
    );

    // Without force: the missing file comes back, the edited one is kept.
    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    assert!(plan.requires_force());
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!((outcome.repaired, outcome.skipped_force), (1, 1));
    assert_eq!(fx.read_game_file("Data/meshes/a.nif").unwrap(), b"AAA");
    assert_eq!(fx.read_game_file("Data/meshes/b.nif").unwrap(), b"edited");

    // With force: everything converges back to the recorded state.
    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, true).unwrap();
    assert_eq!(outcome.repaired, 1);
    assert_eq!(fx.read_game_file("Data/meshes/b.nif").unwrap(), b"BBB");
    assert!(verify::report(&fx.ctx, &fx.inst).unwrap().is_clean());
}

#[test]
fn recovers_staging_from_intact_deployed_copy() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("scripts/x.pex", b"orig".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    let staged = staged_path(&fx, m, "scripts/x.pex");
    std::fs::remove_file(&staged).unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(problems(&report), vec![Problem::StagedMissing]);

    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(plan.actions[0].op, "recover-staging");
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(outcome.repaired, 1);
    assert_eq!(std::fs::read(&staged).unwrap(), b"orig");
    assert!(verify::report(&fx.ctx, &fx.inst).unwrap().is_clean());
}

#[test]
fn both_copies_broken_is_unrepairable() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    std::fs::remove_file(staged_path(&fx, m, "f.esp")).unwrap();
    std::fs::remove_file(fx.game_file("Data/f.esp")).unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(report.findings.len(), 2);

    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    assert!(plan.actions.iter().all(|a| a.op == "skip"));
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, true).unwrap();
    assert_eq!(outcome.repaired, 0);
    assert_eq!(outcome.unrepairable, 2);
}

#[test]
fn tampered_backup_is_detected_and_unrepairable() {
    let fx = fixture();
    // Overwrites vanilla Skyrim.esm, creating a backup.
    let m = fx.install_mod("mod-a", &[("Skyrim.esm", b"modded".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    let backup = fx
        .ctx
        .paths
        .backups_dir
        .join(fx.inst.id.to_string())
        .join("Skyrim.esm");
    assert!(backup.is_file());
    std::fs::write(&backup, b"tampered").unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(problems(&report), vec![Problem::BackupModified]);

    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(plan.actions[0].op, "skip");
}

#[test]
fn moved_game_dir_reports_single_finding_and_blocks_repair() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    let moved = fx.game_dir.with_file_name("game-moved");
    std::fs::rename(&fx.game_dir, &moved).unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(problems(&report), vec![Problem::GameDirMissing]);
    assert_eq!(report.checked_deployed, 0);

    let err = verify::plan_repair(&fx.ctx, &fx.inst).unwrap_err();
    assert!(matches!(err, Error::Blocked(_)), "{err}");

    // Move it back: everything is fine again.
    std::fs::rename(&moved, &fx.game_dir).unwrap();
    assert!(verify::report(&fx.ctx, &fx.inst).unwrap().is_clean());
}

#[test]
fn pending_deployment_is_reported_and_blocks_repair() {
    let fx = fixture();
    fx.ctx
        .db
        .conn
        .execute(
            "INSERT INTO deployments (installation_id, kind, status, started_at)
             VALUES (?1, 'deploy', 'running', 0)",
            [fx.inst.id],
        )
        .unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert!(problems(&report).contains(&Problem::InterruptedDeployment));

    let err = verify::plan_repair(&fx.ctx, &fx.inst).unwrap_err();
    assert!(err.to_string().contains("lmm rollback"), "{err}");
}

#[test]
fn symlink_at_deployed_path_needs_force_to_repair() {
    let fx = fixture();
    let m = fx.install_mod("mod-a", &[("f.esp", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    let target = fx.game_file("Data/f.esp");
    std::fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", &target).unwrap();

    let report = verify::report(&fx.ctx, &fx.inst).unwrap();
    assert_eq!(problems(&report), vec![Problem::DeployedTypeChanged]);

    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    assert!(plan.requires_force());
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, true).unwrap();
    assert_eq!(outcome.repaired, 1);
    assert_eq!(fx.read_game_file("Data/f.esp").unwrap(), b"AAA");
    assert!(!fx.game_file("Data/f.esp").is_symlink());
}

#[test]
#[cfg(unix)]
fn repair_relinks_under_hardlink_method() {
    use lmm_core::config::DeployMethod;
    use std::os::unix::fs::MetadataExt;

    let mut fx = fixture();
    fx.ctx.config.deploy.method = DeployMethod::Hardlink;
    let m = fx.install_mod("mod-a", &[("meshes/a.nif", b"AAA".as_slice())]);
    mods::set_enabled(&fx.ctx.db, fx.active_profile(), &[m], true).unwrap();
    deploy(&fx);

    std::fs::remove_file(fx.game_file("Data/meshes/a.nif")).unwrap();
    let plan = verify::plan_repair(&fx.ctx, &fx.inst).unwrap();
    let outcome = verify::execute_repair(&fx.ctx, &fx.inst, plan, false).unwrap();
    assert_eq!(outcome.repaired, 1);

    // The repaired file is a hard link to the staged copy again.
    let staged = staged_path(&fx, m, "meshes/a.nif");
    let deployed = fx.game_file("Data/meshes/a.nif");
    assert_eq!(
        std::fs::metadata(&staged).unwrap().ino(),
        std::fs::metadata(&deployed).unwrap().ino()
    );
    assert!(verify::report(&fx.ctx, &fx.inst).unwrap().is_clean());
}
