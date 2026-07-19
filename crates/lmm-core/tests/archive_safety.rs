//! Archive validation and extraction safety tests: hostile archives are
//! built in-test and must be rejected without touching anything outside the
//! extraction directory.

#![allow(clippy::unwrap_used)]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use lmm_core::archive;
use lmm_core::config::Limits;
use lmm_core::games;
use lmm_core::staging::import_archive;
use zip::write::SimpleFileOptions;

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zw = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, data) in entries {
        zw.start_file(*name, SimpleFileOptions::default()).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
}

fn extract_to_temp(
    archive_path: &Path,
    limits: &Limits,
) -> (tempfile::TempDir, lmm_core::error::Result<u64>) {
    let dest = tempfile::tempdir().unwrap();
    let res = archive::extract(archive_path, dest.path(), limits);
    (dest, res)
}

#[test]
fn normal_zip_extracts() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("mod.zip");
    write_zip(
        &zip_path,
        &[
            ("Data/MyMod.esp", b"plugin"),
            ("Data/textures/a.dds", b"texture"),
        ],
    );
    let (dest, res) = extract_to_temp(&zip_path, &Limits::default());
    assert_eq!(res.unwrap(), 2);
    assert!(dest.path().join("Data/MyMod.esp").is_file());
    assert!(dest.path().join("Data/textures/a.dds").is_file());
}

#[test]
fn traversal_zip_rejected() {
    let t = tempfile::tempdir().unwrap();
    // The canary file must never be touched.
    let canary = t.path().join("canary.txt");
    fs::write(&canary, b"untouched").unwrap();

    for name in [
        "../canary.txt",
        "a/../../canary.txt",
        "/tmp/abs.txt",
        "C:\\evil.txt",
    ] {
        let zip_path = t.path().join("evil.zip");
        write_zip(&zip_path, &[(name, b"owned"), ("ok.txt", b"ok")]);
        let dest = tempfile::tempdir_in(t.path()).unwrap();
        let err = archive::extract(&zip_path, dest.path(), &Limits::default()).unwrap_err();
        assert!(err.to_string().contains("unsafe archive"), "{name}: {err}");
    }
    assert_eq!(fs::read(&canary).unwrap(), b"untouched");
    assert!(!Path::new("/tmp/abs.txt").exists());
}

#[test]
fn symlink_zip_rejected() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("link.zip");
    let mut zw = zip::ZipWriter::new(File::create(&zip_path).unwrap());
    zw.add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    zw.finish().unwrap();
    let (_dest, res) = extract_to_temp(&zip_path, &Limits::default());
    let err = res.unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
}

#[test]
fn entry_count_bomb_rejected() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("many.zip");
    let entries: Vec<(String, Vec<u8>)> = (0..20).map(|i| (format!("f{i}"), vec![b'x'])).collect();
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    write_zip(&zip_path, &refs);

    let limits = Limits {
        max_archive_entries: 10,
        ..Limits::default()
    };
    let (_dest, res) = extract_to_temp(&zip_path, &limits);
    assert!(res.unwrap_err().to_string().contains("entries"));
}

#[test]
fn total_size_bomb_rejected_by_actual_bytes() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("big.zip");
    // 3 files x 1 MiB of zeros: tiny compressed, 3 MiB decompressed.
    let data = vec![0u8; 1024 * 1024];
    write_zip(&zip_path, &[("a", &data), ("b", &data), ("c", &data)]);
    let limits = Limits {
        max_total_size_mib: 2,
        ..Limits::default()
    };
    let (_dest, res) = extract_to_temp(&zip_path, &limits);
    let err = res.unwrap_err();
    assert!(err.to_string().contains("total size"), "{err}");
}

#[test]
fn per_file_size_bomb_rejected() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("big.zip");
    let data = vec![0u8; 3 * 1024 * 1024];
    write_zip(&zip_path, &[("huge.bin", &data)]);
    let limits = Limits {
        max_file_size_mib: 1,
        ..Limits::default()
    };
    let (_dest, res) = extract_to_temp(&zip_path, &limits);
    assert!(res.unwrap_err().to_string().contains("file size"));
}

#[test]
fn rar_rejected_with_hint_and_garbage_rejected() {
    let t = tempfile::tempdir().unwrap();
    let rar = t.path().join("mod.rar");
    fs::write(&rar, b"Rar!\x1a\x07\x01\x00rest").unwrap();
    let err = archive::detect_kind(&rar).unwrap_err();
    assert!(err.to_string().contains("RAR"), "{err}");

    let junk = t.path().join("mod.zip"); // extension lies
    fs::write(&junk, b"not an archive at all").unwrap();
    assert!(archive::detect_kind(&junk).is_err());
}

#[test]
fn sevenz_roundtrip_extracts() {
    let t = tempfile::tempdir().unwrap();
    let sz_path = t.path().join("mod.7z");
    let mut w = sevenz_rust2::ArchiveWriter::create(&sz_path).unwrap();
    w.push_archive_entry(
        sevenz_rust2::ArchiveEntry::new_file("Data/MyMod.esp"),
        Some(&b"plugin"[..]),
    )
    .unwrap();
    w.push_archive_entry(
        sevenz_rust2::ArchiveEntry::new_file("Data/meshes/m.nif"),
        Some(&b"mesh"[..]),
    )
    .unwrap();
    w.finish().unwrap();

    let (dest, res) = extract_to_temp(&sz_path, &Limits::default());
    assert_eq!(res.unwrap(), 2);
    assert!(dest.path().join("Data/MyMod.esp").is_file());
}

#[test]
fn sevenz_traversal_rejected() {
    let t = tempfile::tempdir().unwrap();
    let sz_path = t.path().join("evil.7z");
    let mut w = sevenz_rust2::ArchiveWriter::create(&sz_path).unwrap();
    w.push_archive_entry(
        sevenz_rust2::ArchiveEntry::new_file("../escape.txt"),
        Some(&b"owned"[..]),
    )
    .unwrap();
    w.finish().unwrap();
    let (_dest, res) = extract_to_temp(&sz_path, &Limits::default());
    assert!(res.unwrap_err().to_string().contains("unsafe archive"));
}

#[test]
fn case_colliding_entries_merge_last_wins() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("case.zip");
    write_zip(
        &zip_path,
        &[("Data/A.txt", b"first"), ("data/a.txt", b"second")],
    );
    let (dest, res) = extract_to_temp(&zip_path, &Limits::default());
    res.unwrap();
    // One file on disk, first-seen casing, last content — the view a
    // case-insensitive game would have.
    assert_eq!(fs::read(dest.path().join("Data/A.txt")).unwrap(), b"second");
    assert!(!dest.path().join("data").exists());
}

#[test]
fn import_pipeline_stages_and_inventories() {
    let t = tempfile::tempdir().unwrap();
    let data_dir = t.path().join("data");
    let paths = lmm_core::config::DataPaths::new(data_dir, None);
    paths.ensure_dirs().unwrap();

    let zip_path = t.path().join("MyMod-1.0.zip");
    write_zip(
        &zip_path,
        &[
            ("MyMod/Data/MyMod.esp", b"plugin"),
            ("MyMod/Data/textures/a.dds", b"texture"),
            ("MyMod/readme.txt", b"docs"),
        ],
    );
    let game = games::by_slug("skyrimse").unwrap();
    let imported = import_archive(&paths, &Limits::default(), game, &zip_path).unwrap();

    // Layout: wrapper dir unwrapped, Data/ found; readme outside Data ignored.
    let rels: Vec<&str> = imported.files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(rels, vec!["MyMod.esp", "textures/a.dds"]);
    assert!(!imported.layout_uncertain);

    // Staged tree exists and matches inventory hashes.
    let staged_root = paths.staging_dir.join(&imported.staging_name);
    for f in &imported.files {
        let p = f.rel.to_native(&staged_root);
        assert!(p.is_file(), "{p:?}");
        assert_eq!(lmm_core::hash::sha256_file(&p).unwrap(), f.sha256);
    }

    // Scratch space fully cleaned up.
    assert_eq!(fs::read_dir(&paths.tmp_dir).unwrap().count(), 0);

    // Removal is safe and idempotent.
    lmm_core::staging::remove_staged(&paths, &imported.staging_name).unwrap();
    assert!(!staged_root.exists());
    lmm_core::staging::remove_staged(&paths, &imported.staging_name).unwrap();
    assert!(lmm_core::staging::remove_staged(&paths, "../oops").is_err());
}

#[test]
fn corrupted_zip_rejected() {
    let t = tempfile::tempdir().unwrap();
    let zip_path = t.path().join("corrupt.zip");
    write_zip(&zip_path, &[("a.txt", b"data")]);
    // Truncate mid-file: valid magic, broken structure.
    let full = fs::read(&zip_path).unwrap();
    fs::write(&zip_path, &full[..full.len() / 2]).unwrap();
    let (_dest, res) = extract_to_temp(&zip_path, &Limits::default());
    assert!(res.is_err());
}

fn _assert_types(_: PathBuf) {}
