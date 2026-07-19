//! Game Tools integration tests: managed installs, detection, game
//! configuration, load-order maintenance and the health check.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::PathBuf;

use common::{fixture, write_file};
use lmm_core::installs;
use lmm_core::tools::{
    self, ToolState, gameconfig, health, install as tool_install, loadorder, registry,
};

/// Register a second installation of `slug` with a Proton prefix, returning
/// (installation, game_dir, prefix_dir).
fn add_proton_install(
    f: &common::Fixture,
    slug: &str,
) -> (lmm_core::model::Installation, PathBuf, PathBuf) {
    let base = f.game_dir.parent().unwrap();
    let game_dir = base.join(format!("game-{slug}"));
    fs::create_dir_all(game_dir.join("Data")).unwrap();
    let prefix = base.join(format!("pfx-{slug}"));
    fs::create_dir_all(&prefix).unwrap();
    let inst = installs::add(
        &f.ctx.db,
        &installs::NewInstallation {
            game_slug: slug,
            path: &game_dir,
            source: "manual",
            steam_library: None,
            proton_prefix: Some(&prefix),
            label: Some(slug),
        },
    )
    .unwrap();
    (inst, game_dir, prefix)
}

/// Minimal Skyrim-era TES4 plugin with the given flags and masters.
fn fake_plugin(flags: u32, masters: &[&str]) -> Vec<u8> {
    let mut sub = Vec::new();
    sub.extend_from_slice(b"HEDR");
    sub.extend_from_slice(&12u16.to_le_bytes());
    sub.extend_from_slice(&[0u8; 12]);
    for m in masters {
        sub.extend_from_slice(b"MAST");
        sub.extend_from_slice(&((m.len() + 1) as u16).to_le_bytes());
        sub.extend_from_slice(m.as_bytes());
        sub.push(0);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"TES4");
    out.extend_from_slice(&(sub.len() as u32).to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&44u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&sub);
    out
}

fn tool(inst: &lmm_core::model::Installation, id: &str) -> &'static registry::ToolDef {
    tools::find_tool(inst, id).unwrap()
}

#[test]
fn install_verify_update_remove_roundtrip() {
    let f = fixture();
    let skse = tool(&f.inst, "skse");

    // Pre-existing unmanaged file the tool will displace.
    write_file(&f.game_file("Data/Scripts/Actor.pex"), b"original-vanilla");

    // Typical extender archive: everything under a versioned wrapper dir.
    let zip = f.make_zip(
        "skse64_2_02_06.zip",
        &[
            ("skse64_2_02_06/skse64_loader.exe", b"loader".as_slice()),
            ("skse64_2_02_06/skse64_1_6_1170.dll", b"dll"),
            ("skse64_2_02_06/Data/Scripts/Actor.pex", b"skse-pex"),
        ],
    );
    let installed = tool_install::install(&f.ctx, &f.inst, skse, &zip, None, false).unwrap();
    assert_eq!(installed.files, 3);
    assert_eq!(installed.backed_up, 1, "displaced vanilla file backed up");
    assert_eq!(installed.version.as_deref(), Some("2.02.06"));
    assert_eq!(f.read_game_file("skse64_loader.exe").unwrap(), b"loader");
    assert_eq!(
        f.read_game_file("Data/Scripts/Actor.pex").unwrap(),
        b"skse-pex"
    );

    let st = tools::tool_status(&f.ctx, &f.inst, skse).unwrap();
    assert_eq!(st.state, ToolState::Installed);
    assert!(st.managed);

    // Verify: clean, then detect an external edit.
    let ok = tool_install::verify(&f.ctx, &f.inst, skse).unwrap();
    assert!(ok.iter().all(|v| v.state == tool_install::FileState::Ok));
    fs::write(f.game_file("skse64_1_6_1170.dll"), b"tampered").unwrap();
    let report = tool_install::verify(&f.ctx, &f.inst, skse).unwrap();
    assert!(
        report
            .iter()
            .any(|v| v.state == tool_install::FileState::Modified)
    );

    // Update refuses to clobber the drifted file without force...
    let zip2 = f.make_zip(
        "skse64_2_02_07.zip",
        &[
            ("skse64_2_02_07/skse64_loader.exe", b"loader-v2".as_slice()),
            ("skse64_2_02_07/skse64_1_6_1170.dll", b"dll-v2"),
        ],
    );
    let err = tool_install::install(&f.ctx, &f.inst, skse, &zip2, None, false).unwrap_err();
    assert!(err.to_string().contains("modified outside lmm"), "{err}");

    // ...and with force replaces it, removing the file dropped by the new
    // version and restoring what that file had displaced.
    let updated = tool_install::install(&f.ctx, &f.inst, skse, &zip2, None, true).unwrap();
    assert_eq!(updated.files, 2);
    assert_eq!(updated.stale_removed, 1);
    assert_eq!(f.read_game_file("skse64_loader.exe").unwrap(), b"loader-v2");
    assert_eq!(
        f.read_game_file("Data/Scripts/Actor.pex").unwrap(),
        b"original-vanilla",
        "backup of the displaced original restored when the new version stopped shipping it"
    );

    // Remove: managed files gone, nothing else touched.
    let removed = tool_install::remove(&f.ctx, &f.inst, skse, false).unwrap();
    assert_eq!(removed.removed, 2);
    assert!(f.read_game_file("skse64_loader.exe").is_none());
    assert!(f.read_game_file("SkyrimSE.exe").is_some(), "game untouched");
    assert_eq!(
        tools::tool_status(&f.ctx, &f.inst, skse).unwrap().state,
        ToolState::Missing
    );
}

#[test]
fn install_is_undone_when_it_fails_midway() {
    let f = fixture();
    let skse = tool(&f.inst, "skse");
    // A directory where the tool wants a file makes the write fail.
    fs::create_dir_all(f.game_file("skse64_loader.exe")).unwrap();
    write_file(&f.game_file("Data/Scripts/Actor.pex"), b"original");
    let zip = f.make_zip(
        "skse.zip",
        &[
            // Sorted inventory order puts the Data file before the loader,
            // so the failure happens after at least one successful write.
            ("Data/Scripts/Actor.pex", b"skse-pex".as_slice()),
            ("skse64_loader.exe", b"loader"),
        ],
    );
    assert!(tool_install::install(&f.ctx, &f.inst, skse, &zip, None, false).is_err());
    assert_eq!(
        f.read_game_file("Data/Scripts/Actor.pex").unwrap(),
        b"original",
        "displaced file put back after the failed install"
    );
    assert!(
        tool_install::get_record(&f.ctx.db, f.inst.id, "skse")
            .unwrap()
            .is_none(),
        "nothing recorded"
    );
}

#[test]
fn install_refuses_paths_deployed_by_mods() {
    let f = fixture();
    // A deployed mod file at the same path the tool would write.
    let mod_id = f.install_mod(
        "addrlib-as-mod",
        &[("SKSE/Plugins/version-1-6-1170-0.bin", b"x")],
    );
    f.ctx
        .db
        .conn
        .execute(
            "INSERT INTO deployed_files (installation_id, path_key, rel_path, provider_mod_id, sha256)
             VALUES (?1, ?2, ?3, ?4, 'h')",
            rusqlite::params![
                f.inst.id,
                "skse/plugins/version-1-6-1170-0.bin",
                "SKSE/Plugins/version-1-6-1170-0.bin",
                mod_id
            ],
        )
        .unwrap();

    let addr = tool(&f.inst, "address-library");
    let zip = f.make_zip(
        "addrlib.zip",
        &[("SKSE/Plugins/version-1-6-1170-0.bin", b"y".as_slice())],
    );
    let err = tool_install::install(&f.ctx, &f.inst, addr, &zip, None, false).unwrap_err();
    assert!(err.to_string().contains("deployed by a mod"), "{err}");
}

#[test]
fn unmanaged_tools_are_detected_on_disk() {
    let f = fixture();
    let skse = tool(&f.inst, "skse");
    let addr = tool(&f.inst, "address-library");

    assert_eq!(
        tools::tool_status(&f.ctx, &f.inst, skse).unwrap().state,
        ToolState::Missing
    );
    write_file(&f.game_file("skse64_loader.exe"), b"external");
    let st = tools::tool_status(&f.ctx, &f.inst, skse).unwrap();
    assert_eq!(st.state, ToolState::Installed);
    assert!(!st.managed);

    // Wildcard detection for the address library's versioned file.
    write_file(
        &f.game_file("Data/SKSE/Plugins/version-1-6-1170-0.bin"),
        b"x",
    );
    assert_eq!(
        tools::tool_status(&f.ctx, &f.inst, addr).unwrap().state,
        ToolState::Installed
    );
}

#[test]
fn outdated_is_reported_from_recorded_version() {
    let f = fixture();
    let skse = tool(&f.inst, "skse"); // latest_known 2.2.6
    let zip = f.make_zip(
        "skse64_2_01_05.zip",
        &[("skse64_loader.exe", b"x".as_slice())],
    );
    tool_install::install(&f.ctx, &f.inst, skse, &zip, None, false).unwrap();
    let st = tools::tool_status(&f.ctx, &f.inst, skse).unwrap();
    assert_eq!(st.state, ToolState::Outdated);
    assert!(st.detail.unwrap().contains("2.01.05"));
}

#[test]
fn standalone_tools_live_in_the_data_dir() {
    let f = fixture();
    let loot = tool(&f.inst, "loot");
    let zip = f.make_zip("loot.zip", &[("LOOT.exe", b"loot".as_slice())]);
    let installed = tool_install::install(&f.ctx, &f.inst, loot, &zip, None, false).unwrap();
    assert!(installed.target_root.starts_with(&f.ctx.paths.tools_dir));
    assert!(installed.target_root.join("LOOT.exe").is_file());
    assert!(
        !f.game_file("LOOT.exe").exists(),
        "standalone tools never touch the game directory"
    );
    tool_install::remove(&f.ctx, &f.inst, loot, false).unwrap();
    assert!(!installed.target_root.exists(), "empty tool dir cleaned up");
}

#[test]
fn installation_remove_is_blocked_while_tools_installed() {
    let f = fixture();
    let skse = tool(&f.inst, "skse");
    let zip = f.make_zip("skse.zip", &[("skse64_loader.exe", b"x".as_slice())]);
    tool_install::install(&f.ctx, &f.inst, skse, &zip, None, false).unwrap();
    let err = installs::remove(&f.ctx.db, f.inst.id).unwrap_err();
    assert!(err.to_string().contains("tool"), "{err}");
    tool_install::remove(&f.ctx, &f.inst, skse, false).unwrap();
    installs::remove(&f.ctx.db, f.inst.id).unwrap();
}

#[test]
fn gameconfig_detect_apply_restore() {
    let f = fixture();
    let (inst, _game_dir, prefix) = add_proton_install(&f, "falloutnv");
    let game = tools::catalog(&inst).unwrap();
    let ini_dir = prefix.join("drive_c/users/steamuser/Documents/My Games/FalloutNV");
    let ini = ini_dir.join("Fallout.ini");
    write_file(
        &ini,
        b"[General]\nsName=Player\n[Archive]\nbInvalidateOlderFiles=0\nSInvalidationFile=Archive.txt\n",
    );

    let st = gameconfig::status(&inst, game).unwrap();
    assert!(
        st.iter()
            .all(|t| !matches!(t.state, gameconfig::TweakState::Applied))
    );

    let applied = gameconfig::apply(&f.ctx, &inst, game).unwrap();
    assert_eq!(applied.applied.len(), 2);
    assert_eq!(applied.backed_up, vec!["Fallout.ini"]);
    let text = fs::read_to_string(&ini).unwrap();
    assert!(text.contains("bInvalidateOlderFiles=1"));
    assert!(text.contains("SInvalidationFile=\n") || text.ends_with("SInvalidationFile="));
    assert!(text.contains("sName=Player"), "unrelated lines preserved");

    // Idempotent: nothing more to do, no second backup.
    let again = gameconfig::apply(&f.ctx, &inst, game).unwrap();
    assert!(again.applied.is_empty() && again.backed_up.is_empty());

    let st = gameconfig::status(&inst, game).unwrap();
    assert!(
        st.iter()
            .all(|t| matches!(t.state, gameconfig::TweakState::Applied))
    );

    // Restore brings back the byte-exact original.
    let restored = gameconfig::restore(&f.ctx, &inst, game).unwrap();
    assert_eq!(restored.restored, vec!["Fallout.ini"]);
    let text = fs::read_to_string(&ini).unwrap();
    assert!(text.contains("bInvalidateOlderFiles=0"));
    // A second restore has nothing recorded to undo.
    assert!(gameconfig::restore(&f.ctx, &inst, game).is_err());
}

#[test]
fn gameconfig_creates_and_deletes_missing_custom_ini() {
    let f = fixture();
    let (inst, _game_dir, prefix) = add_proton_install(&f, "fallout4");
    let game = tools::catalog(&inst).unwrap();
    let ini_dir = prefix.join("drive_c/users/steamuser/Documents/My Games/Fallout4");
    fs::create_dir_all(&ini_dir).unwrap();

    let applied = gameconfig::apply(&f.ctx, &inst, game).unwrap();
    assert_eq!(applied.applied.len(), 3);
    let custom = fs::read_to_string(ini_dir.join("Fallout4Custom.ini")).unwrap();
    assert!(custom.contains("[Archive]"));
    assert!(custom.contains("bInvalidateOlderFiles=1"));

    let restored = gameconfig::restore(&f.ctx, &inst, game).unwrap();
    // Both files were created by lmm, so restore deletes them.
    assert_eq!(restored.restored, Vec::<String>::new());
    assert_eq!(restored.deleted.len(), 2);
    assert!(!ini_dir.join("Fallout4Custom.ini").exists());
}

#[test]
fn loadorder_analyze_sort_and_restore() {
    let f = fixture();
    let (inst, game_dir, prefix) = add_proton_install(&f, "skyrimse");
    let game = tools::catalog(&inst).unwrap();
    let data = game_dir.join("Data");
    write_file(&data.join("Skyrim.esm"), b"not-a-real-esm");
    fs::write(data.join("B.esm"), fake_plugin(0x1, &[])).unwrap();
    fs::write(data.join("PatchB.esp"), fake_plugin(0, &["B.esm"])).unwrap();
    fs::write(data.join("Unlisted.esp"), fake_plugin(0, &[])).unwrap();
    let plugins_txt =
        prefix.join("drive_c/users/steamuser/AppData/Local/Skyrim Special Edition/plugins.txt");
    write_file(
        &plugins_txt,
        b"*PatchB.esp\n*Skyrim.esm\n*B.esm\nDisabled.esp\n*Gone.esp\n",
    );

    let a = loadorder::analyze(&inst, game).unwrap();
    assert_eq!(a.plugins.len(), 5);
    assert_eq!(a.unlisted, vec!["Unlisted.esp"]);
    assert!(
        a.issues
            .iter()
            .any(|i| i.kind == loadorder::IssueKind::MissingFile && i.plugin == "Gone.esp")
    );
    assert!(
        a.issues
            .iter()
            .any(|i| i.kind == loadorder::IssueKind::MasterOutOfOrder && i.plugin == "PatchB.esp")
    );

    let plan = loadorder::plan_sort(&a, game);
    assert!(plan.changed);
    // Official master first, then other masters, dependents after their
    // masters, everything else in stable order.
    let pos = |n: &str| plan.after.iter().position(|x| x == n).unwrap();
    assert_eq!(pos("Skyrim.esm"), 0);
    assert!(pos("B.esm") < pos("PatchB.esp"));

    let backup = loadorder::apply_sort(&f.ctx, &inst, game, &a, &plan).unwrap();
    assert!(backup.exists());
    let text = fs::read_to_string(&plugins_txt).unwrap();
    assert!(text.starts_with("*Skyrim.esm\n"));
    assert!(
        text.contains("\nDisabled.esp\n"),
        "disabled state survives the sort"
    );
}

#[test]
fn loadorder_restore_returns_to_presort_order() {
    let f = fixture();
    let (inst, game_dir, prefix) = add_proton_install(&f, "skyrimse");
    let game = tools::catalog(&inst).unwrap();
    fs::write(game_dir.join("Data/B.esm"), fake_plugin(0x1, &[])).unwrap();
    let plugins_txt =
        prefix.join("drive_c/users/steamuser/AppData/Local/Skyrim Special Edition/plugins.txt");
    let original = b"*B.esm\n*Skyrim.esm\n".as_slice();
    write_file(&plugins_txt, original);
    write_file(&game_dir.join("Data/Skyrim.esm"), b"esm");

    let a = loadorder::analyze(&inst, game).unwrap();
    let plan = loadorder::plan_sort(&a, game);
    loadorder::apply_sort(&f.ctx, &inst, game, &a, &plan).unwrap();
    assert_ne!(fs::read(&plugins_txt).unwrap(), original);

    loadorder::restore(&f.ctx, &inst, game, None).unwrap();
    assert_eq!(fs::read(&plugins_txt).unwrap(), original);
    assert!(loadorder::backups(&f.ctx, &inst).unwrap().len() >= 2);
}

#[test]
fn health_check_flags_missing_extender_then_clears() {
    let f = fixture();
    let game = tools::catalog(&f.inst).unwrap();
    let checks = health::run(&f.ctx, &f.inst, game).unwrap();
    let extender = checks.iter().find(|c| c.name == "script extender").unwrap();
    assert_eq!(extender.status, health::CheckStatus::Fail);
    assert!(extender.recommendation.is_some());

    write_file(&f.game_file("skse64_loader.exe"), b"x");
    let checks = health::run(&f.ctx, &f.inst, game).unwrap();
    let extender = checks.iter().find(|c| c.name == "script extender").unwrap();
    assert_eq!(extender.status, health::CheckStatus::Ok);

    // No Proton prefix: the plugin-list check is skipped, not failed.
    let plugins = checks.iter().find(|c| c.name == "plugin list").unwrap();
    assert_eq!(plugins.status, health::CheckStatus::Skip);
}
