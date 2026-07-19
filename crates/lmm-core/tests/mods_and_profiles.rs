//! Mod installation, load order, conflicts, and profile behavior.

#![allow(clippy::unwrap_used)]

mod common;

use common::fixture;
use lmm_core::{mods, profile, resolve};

#[test]
fn install_inventories_files_relative_to_data() {
    let fx = fixture();
    let id = fx.install_mod(
        "Alpha",
        &[
            ("Data/Alpha.esp", b"alpha-plugin"),
            ("Data/textures/a.dds", b"alpha-texture"),
        ],
    );
    let files = mods::files(&fx.ctx.db, id).unwrap();
    let rels: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(rels, vec!["Alpha.esp", "textures/a.dds"]);

    // New mods are disabled and last in load order.
    let list = mods::list_for_profile(&fx.ctx.db, fx.active_profile()).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].enabled);
    assert_eq!(list[0].priority, 1);
}

#[test]
fn duplicate_name_and_archive_rejected() {
    let fx = fixture();
    fx.install_mod("Alpha", &[("Data/Alpha.esp", b"same-bytes")]);

    let zip = fx.make_zip("alpha2.zip", &[("Data/Alpha.esp", b"same-bytes")]);
    // Same name refused (case-insensitive).
    let err = mods::install(
        &fx.ctx,
        &fx.inst,
        &zip,
        &mods::InstallOptions {
            name: Some("alpha"),
            version: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("already installed"), "{err}");

    // Same archive bytes under a new name also refused.
    let err = mods::install(
        &fx.ctx,
        &fx.inst,
        &zip,
        &mods::InstallOptions {
            name: Some("AlphaCopy"),
            version: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("exact archive"), "{err}");
    // The failed install left no staging orphan behind.
    let staged = std::fs::read_dir(&fx.ctx.paths.staging_dir)
        .unwrap()
        .count();
    assert_eq!(staged, 1);
}

#[test]
fn conflicts_follow_load_order_and_reorder() {
    let fx = fixture();
    let a = fx.install_mod("Alpha", &[("Data/textures/shared.dds", b"from-alpha")]);
    let b = fx.install_mod("Beta", &[("Data/Textures/Shared.dds", b"from-beta")]);
    let profile_id = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, profile_id, &[a, b], true).unwrap();

    // Beta installed later -> higher priority -> wins, despite differing case.
    let conflicts = resolve::conflicts(&fx.ctx.db, profile_id).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path_key, "textures/shared.dds");
    assert_eq!(conflicts[0].providers[0].mod_name, "Beta");

    let desired = resolve::desired_state(&fx.ctx.db, profile_id).unwrap();
    assert_eq!(desired["textures/shared.dds"].mod_name, "Beta");

    // Move Alpha to the end of the load order: Alpha now wins.
    mods::set_position(&fx.ctx.db, profile_id, a, 2).unwrap();
    let desired = resolve::desired_state(&fx.ctx.db, profile_id).unwrap();
    assert_eq!(desired["textures/shared.dds"].mod_name, "Alpha");

    // Disabling the winner restores the next provider.
    mods::set_enabled(&fx.ctx.db, profile_id, &[a], false).unwrap();
    let desired = resolve::desired_state(&fx.ctx.db, profile_id).unwrap();
    assert_eq!(desired["textures/shared.dds"].mod_name, "Beta");
    assert!(
        resolve::conflicts(&fx.ctx.db, profile_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn profiles_isolate_enabled_state_and_copy() {
    let fx = fixture();
    let a = fx.install_mod("Alpha", &[("Data/a.esp", b"a")]);
    let default_profile = fx.active_profile();
    mods::set_enabled(&fx.ctx.db, default_profile, &[a], true).unwrap();

    // New profile: mod present but disabled.
    let fresh = profile::create(&fx.ctx.db, fx.inst.id, "fresh").unwrap();
    let list = mods::list_for_profile(&fx.ctx.db, fresh.id).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].enabled);

    // Copy of default keeps enabled state.
    let copy = profile::copy(&fx.ctx.db, fx.inst.id, "default", "copy").unwrap();
    let list = mods::list_for_profile(&fx.ctx.db, copy.id).unwrap();
    assert!(list[0].enabled);

    // Install after profile creation: appears in all profiles, disabled.
    let b = fx.install_mod("Beta", &[("Data/b.esp", b"b")]);
    for pid in [default_profile, fresh.id, copy.id] {
        let list = mods::list_for_profile(&fx.ctx.db, pid).unwrap();
        let beta = list.iter().find(|m| m.info.id == b).unwrap();
        assert!(!beta.enabled, "profile {pid}");
        assert_eq!(beta.priority, 2);
    }

    // Active profile management.
    let switched = profile::switch(&fx.ctx.db, fx.inst.id, "fresh").unwrap();
    assert!(switched.is_active);
    let err = profile::delete(&fx.ctx.db, fx.inst.id, "fresh").unwrap_err();
    assert!(err.to_string().contains("active"), "{err}");
    profile::delete(&fx.ctx.db, fx.inst.id, "copy").unwrap();
    assert_eq!(profile::list(&fx.ctx.db, fx.inst.id).unwrap().len(), 2);
}

#[test]
fn uninstall_removes_staging_and_rows() {
    let fx = fixture();
    let a = fx.install_mod("Alpha", &[("Data/a.esp", b"a")]);
    let staging_dir = fx.ctx.paths.staging_dir.clone();
    assert_eq!(std::fs::read_dir(&staging_dir).unwrap().count(), 1);

    mods::uninstall(&fx.ctx, &fx.inst, a).unwrap();
    assert_eq!(std::fs::read_dir(&staging_dir).unwrap().count(), 0);
    assert!(mods::get(&fx.ctx.db, a).is_err());
    assert!(
        mods::list_for_profile(&fx.ctx.db, fx.active_profile())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn find_by_id_name_and_prefix() {
    let fx = fixture();
    let a = fx.install_mod("Alpha Textures", &[("Data/a.esp", b"a")]);
    fx.install_mod("Alpha Meshes", &[("Data/b.esp", b"b")]);

    assert_eq!(
        mods::find(&fx.ctx.db, fx.inst.id, &a.to_string())
            .unwrap()
            .id,
        a
    );
    assert_eq!(
        mods::find(&fx.ctx.db, fx.inst.id, "alpha textures")
            .unwrap()
            .id,
        a
    );
    assert_eq!(mods::find(&fx.ctx.db, fx.inst.id, "alpha t").unwrap().id, a);
    assert!(mods::find(&fx.ctx.db, fx.inst.id, "alpha").is_err()); // ambiguous
    assert!(mods::find(&fx.ctx.db, fx.inst.id, "nope").is_err());
}
