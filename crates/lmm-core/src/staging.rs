//! Staging: the canonical on-disk copy of every installed mod.
//!
//! Import pipeline: hash archive -> extract into a scratch dir under the data
//! dir (bounded, validated) -> detect layout -> inventory (hash every file)
//! -> rename the detected root into `staging/<name>`. The game directory is
//! never involved; deployment later copies from staging.

use std::fs;
use std::path::Path;

use crate::archive;
use crate::config::{DataPaths, Limits};
use crate::db::now;
use crate::error::{Error, IoContext, Result};
use crate::games::GameDef;
use crate::hash::sha256_file;
use crate::paths::RelPath;

#[derive(Debug, Clone)]
pub struct StagedFile {
    pub rel: RelPath,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct ImportedMod {
    /// Directory name under `staging/` now holding the mod's files.
    pub staging_name: String,
    pub archive_sha256: String,
    pub files: Vec<StagedFile>,
    pub layout_rule: &'static str,
    pub layout_uncertain: bool,
}

/// An archive extracted into a scratch directory, before layout detection
/// or FOMOD handling. Dropping it removes the scratch tree.
pub struct Extracted {
    pub scratch: tempfile::TempDir,
    pub archive_sha256: String,
}

impl Extracted {
    pub fn root(&self) -> &Path {
        self.scratch.path()
    }
}

/// Phase 1 of every import: hash, then extract with limits into a scratch
/// directory under the data dir.
pub fn extract_archive(
    paths: &DataPaths,
    limits: &Limits,
    archive_path: &Path,
) -> Result<Extracted> {
    let archive_sha256 = sha256_file(archive_path)?;
    // TempDir removes the scratch tree on any error path.
    let scratch = tempfile::tempdir_in(&paths.tmp_dir).path_ctx(&paths.tmp_dir)?;
    archive::extract(archive_path, scratch.path(), limits)?;
    Ok(Extracted {
        scratch,
        archive_sha256,
    })
}

/// Phase 2, plain archives: detect the layout and promote the detected
/// root into staging.
pub fn finish_import(
    paths: &DataPaths,
    game: &GameDef,
    extracted: &Extracted,
) -> Result<ImportedMod> {
    let layout = archive::detect_mod_root(extracted.root(), game)?;
    let files = inventory(&layout.root)?;
    if files.is_empty() {
        return Err(Error::Archive(
            "archive contains no usable mod files".into(),
        ));
    }
    promote(
        paths,
        &extracted.archive_sha256,
        &layout.root,
        files,
        layout.rule,
        layout.uncertain,
    )
}

/// Phase 2, FOMOD installs: materialize a validated plan (copy each
/// planned source to its destination in a fresh tree) and promote that.
/// Copies rather than moves because several destinations may share one
/// archive source.
pub fn finish_import_planned(
    paths: &DataPaths,
    extracted: &Extracted,
    plan: &[crate::fomod::plan::PlannedFile],
    installer_root: &Path,
) -> Result<ImportedMod> {
    if plan.is_empty() {
        return Err(Error::Fomod(
            "these selections install no files at all".into(),
        ));
    }
    let build = tempfile::tempdir_in(&paths.tmp_dir).path_ctx(&paths.tmp_dir)?;
    for f in plan {
        let src = f.source.to_native(installer_root);
        let dst = f.dest.to_native(build.path());
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).path_ctx(parent)?;
        }
        fs::copy(&src, &dst).path_ctx(&src)?;
    }
    let files = inventory(build.path())?;
    promote(
        paths,
        &extracted.archive_sha256,
        build.path(),
        files,
        "fomod installer selections",
        false,
    )
}

/// Common tail: atomically rename a finished tree into staging/.
fn promote(
    paths: &DataPaths,
    archive_sha256: &str,
    root: &Path,
    files: Vec<StagedFile>,
    layout_rule: &'static str,
    layout_uncertain: bool,
) -> Result<ImportedMod> {
    // Same filesystem by construction (tmp/ and staging/ are siblings in
    // the data dir), so this is an atomic rename: staging never contains a
    // half-imported mod.
    let staging_name = claim_staging_name(paths, archive_sha256)?;
    let target = paths.staging_dir.join(&staging_name);
    fs::rename(root, &target).path_ctx(&target)?;
    Ok(ImportedMod {
        staging_name,
        archive_sha256: archive_sha256.to_string(),
        files,
        layout_rule,
        layout_uncertain,
    })
}

pub fn import_archive(
    paths: &DataPaths,
    limits: &Limits,
    game: &GameDef,
    archive_path: &Path,
) -> Result<ImportedMod> {
    let extracted = extract_archive(paths, limits, archive_path)?;
    finish_import(paths, game, &extracted)
}

/// Hash and record every regular file under `root`. Also used by the tool
/// installer, which shares the archive-import pipeline but not staging.
pub(crate) fn inventory(root: &Path) -> Result<Vec<StagedFile>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|e| {
            Error::Invalid(format!(
                "walking staged files under {}: {e}",
                root.display()
            ))
        })?;
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        // Extraction only creates regular files; anything else appearing
        // here means tampering or a bug — refuse to continue.
        if !ft.is_file() {
            return Err(Error::Invalid(format!(
                "unexpected non-regular file in extracted tree: {}",
                entry.path().display()
            )));
        }
        let rel_os = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| Error::Invalid("walkdir escaped root".into()))?;
        let rel = RelPath::from_os_rel(rel_os)?;
        let size = entry
            .metadata()
            .map_err(|e| Error::Invalid(format!("stat {}: {e}", entry.path().display())))?
            .len();
        let sha256 = sha256_file(entry.path())?;
        files.push(StagedFile { rel, size, sha256 });
    }
    Ok(files)
}

/// Reserve a unique directory name in staging/. `create_dir` is the atomic
/// claim; the empty dir is immediately replaced by the rename in the caller.
fn claim_staging_name(paths: &DataPaths, archive_sha256: &str) -> Result<String> {
    let stem = format!("m-{}-{}", now(), &archive_sha256[..12]);
    for attempt in 0..100u32 {
        let name = if attempt == 0 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        };
        let path = paths.staging_dir.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => {
                // Claimed; remove the placeholder so rename() can take the spot.
                fs::remove_dir(&path).path_ctx(&path)?;
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::io(&path, e)),
        }
    }
    Err(Error::Invalid(
        "could not allocate staging directory".into(),
    ))
}

/// Delete a mod's staged files. The name comes from our own database, but is
/// still validated: it must be a plain directory name inside staging/.
pub fn remove_staged(paths: &DataPaths, staging_name: &str) -> Result<()> {
    if staging_name.is_empty()
        || staging_name.contains('/')
        || staging_name.contains('\\')
        || staging_name.starts_with('.')
    {
        return Err(Error::Invalid(format!(
            "refusing to delete suspicious staging dir name '{staging_name}'"
        )));
    }
    let path = paths.staging_dir.join(staging_name);
    match fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        // Already gone: removal is idempotent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(&path, e)),
    }
}
