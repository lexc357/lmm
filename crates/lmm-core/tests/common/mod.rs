//! Shared test fixture: an isolated Context (temp data dir + temp db),
//! a fake Skyrim SE game directory, and zip helpers.

#![allow(clippy::unwrap_used, dead_code)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use lmm_core::model::Installation;
use lmm_core::{Context, Overrides, installs};
use zip::write::SimpleFileOptions;

pub struct Fixture {
    pub ctx: Context,
    pub game_dir: PathBuf,
    pub inst: Installation,
    // Held for cleanup on drop.
    _tmp: tempfile::TempDir,
}

pub fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let game_dir = tmp.path().join("game");
    // A minimal believable Skyrim SE tree with one vanilla file.
    std::fs::create_dir_all(game_dir.join("Data")).unwrap();
    std::fs::write(game_dir.join("SkyrimSE.exe"), b"exe").unwrap();
    std::fs::write(game_dir.join("Data/Skyrim.esm"), b"vanilla-esm").unwrap();

    let ctx = Context::open(&Overrides {
        config_path: Some(tmp.path().join("no-config.toml")),
        data_dir: Some(tmp.path().join("data")),
        db_path: None,
    })
    .unwrap();

    let inst = installs::add(
        &ctx.db,
        &installs::NewInstallation {
            game_slug: "skyrimse",
            path: &game_dir,
            source: "manual",
            steam_library: None,
            proton_prefix: None,
            label: Some("test"),
        },
    )
    .unwrap();

    Fixture {
        ctx,
        game_dir: game_dir.canonicalize().unwrap(),
        inst,
        _tmp: tmp,
    }
}

impl Fixture {
    pub fn active_profile(&self) -> i64 {
        self.inst.active_profile_id.unwrap()
    }

    /// Build a zip archive in the temp dir and return its path.
    pub fn make_zip(&self, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = self._tmp.path().join(name);
        let mut zw = zip::ZipWriter::new(File::create(&path).unwrap());
        for (entry_name, data) in entries {
            zw.start_file(*entry_name, SimpleFileOptions::default())
                .unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    /// Install a zip with `Data/`-relative entries and return the mod id.
    pub fn install_mod(&self, name: &str, entries: &[(&str, &[u8])]) -> i64 {
        let zip = self.make_zip(&format!("{name}.zip"), entries);
        lmm_core::mods::install(
            &self.ctx,
            &self.inst,
            &zip,
            &lmm_core::mods::InstallOptions {
                name: Some(name),
                version: None,
            },
        )
        .unwrap()
        .info
        .id
    }

    pub fn game_file(&self, rel: &str) -> PathBuf {
        self.game_dir.join(rel)
    }

    pub fn read_game_file(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.game_file(rel)).ok()
    }
}

pub fn write_file(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}
