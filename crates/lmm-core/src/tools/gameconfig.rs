//! Game configuration for modding: detect, apply and restore the INI
//! settings a game needs before mods work (archive invalidation, loose
//! files, plugin selection).
//!
//! Edits are minimal and line-preserving: only the targeted key changes,
//! everything else in the file — comments, ordering, unrelated sections —
//! stays byte-identical. The first time lmm touches a file the original is
//! copied into the backups directory and recorded, so `restore` can always
//! return to the pre-lmm state.

use std::fs;
use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Context;
use crate::db::now;
use crate::error::{Error, IoContext, Result};
use crate::hash::sha256_file;
use crate::model::Installation;
use crate::tools::registry::{GameTools, IniDir, TweakDef};

/// Where an installation keeps its INI files.
pub fn ini_dir(inst: &Installation, game: &GameTools) -> Result<PathBuf> {
    match game.ini_dir {
        None => Err(Error::Invalid(format!(
            "'{}' needs no INI configuration for modding",
            inst.game_slug
        ))),
        Some(IniDir::GameRoot) => Ok(inst.path.clone()),
        Some(IniDir::MyGames(name)) => {
            let prefix = inst.proton_prefix.as_ref().ok_or_else(|| {
                Error::Invalid(
                    "this installation has no Proton prefix; run the game once through Steam \
                     so it creates its configuration files"
                        .into(),
                )
            })?;
            Ok(prefix
                .join("drive_c/users/steamuser/Documents/My Games")
                .join(name))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum TweakState {
    Applied,
    NotApplied {
        current: Option<String>,
    },
    /// The INI file (or the whole INI directory) does not exist yet.
    FileMissing,
}

#[derive(Debug, Clone, Serialize)]
pub struct TweakStatus {
    pub id: String,
    pub file: String,
    pub section: String,
    pub key: String,
    pub value: String,
    pub why: String,
    #[serde(flatten)]
    pub state: TweakState,
}

fn tweak_status(dir: &std::path::Path, t: &TweakDef) -> TweakStatus {
    let state = match resolve_file(dir, t.file) {
        None => TweakState::FileMissing,
        Some(path) => match fs::read_to_string(&path) {
            Err(_) => TweakState::FileMissing,
            Ok(text) => match get_value(&text, t.section, t.key) {
                Some(v) if v == t.value => TweakState::Applied,
                other => TweakState::NotApplied { current: other },
            },
        },
    };
    TweakStatus {
        id: t.id.to_string(),
        file: t.file.to_string(),
        section: t.section.to_string(),
        key: t.key.to_string(),
        value: t.value.to_string(),
        why: t.why.to_string(),
        state,
    }
}

/// Status of every tweak the game defines. Empty = nothing to configure.
pub fn status(inst: &Installation, game: &GameTools) -> Result<Vec<TweakStatus>> {
    if game.tweaks.is_empty() {
        return Ok(Vec::new());
    }
    let dir = ini_dir(inst, game)?;
    Ok(game.tweaks.iter().map(|t| tweak_status(&dir, t)).collect())
}

#[derive(Debug, Serialize)]
pub struct AppliedConfig {
    /// Tweak ids that were changed (already-applied ones are not listed).
    pub applied: Vec<String>,
    /// Files backed up for the first time.
    pub backed_up: Vec<String>,
}

/// Apply every missing tweak. Each touched file is backed up (once, the
/// first time lmm ever modifies it) before the edit.
pub fn apply(ctx: &Context, inst: &Installation, game: &GameTools) -> Result<AppliedConfig> {
    let dir = ini_dir(inst, game)?;
    fs::create_dir_all(&dir).path_ctx(&dir)?;
    let mut out = AppliedConfig {
        applied: Vec::new(),
        backed_up: Vec::new(),
    };
    for t in game.tweaks {
        let path = resolve_file(&dir, t.file).unwrap_or_else(|| dir.join(t.file));
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Error::io(&path, e)),
        };
        if get_value(&text, t.section, t.key).as_deref() == Some(t.value) {
            continue;
        }
        if backup_once(ctx, inst, t.file, &path)? {
            out.backed_up.push(t.file.to_string());
        }
        let new_text = set_value(&text, t.section, t.key, t.value);
        let tmp = path.with_extension("lmm-tmp");
        fs::write(&tmp, &new_text).path_ctx(&tmp)?;
        fs::rename(&tmp, &path).path_ctx(&path)?;
        out.applied.push(t.id.to_string());
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct RestoredConfig {
    /// Files put back to their pre-lmm content.
    pub restored: Vec<String>,
    /// Files lmm created from scratch, now deleted.
    pub deleted: Vec<String>,
}

/// Undo every configuration change lmm ever made to this game's INI files:
/// restore recorded backups, delete files lmm created from scratch.
pub fn restore(ctx: &Context, inst: &Installation, game: &GameTools) -> Result<RestoredConfig> {
    let dir = ini_dir(inst, game)?;
    let mut out = RestoredConfig {
        restored: Vec::new(),
        deleted: Vec::new(),
    };
    let rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = ctx.db.conn.prepare(
            "SELECT id, file, backup_path FROM config_backups WHERE installation_id = ?1",
        )?;
        let rows = stmt.query_map([inst.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if rows.is_empty() {
        return Err(Error::Invalid(
            "lmm has not changed any configuration files for this game".into(),
        ));
    }
    for (row_id, file, backup_path) in rows {
        let target = resolve_file(&dir, &file).unwrap_or_else(|| dir.join(&file));
        match backup_path {
            Some(rel) => {
                let backup_abs = ctx.paths.backups_dir.join(&rel);
                fs::copy(&backup_abs, &target).path_ctx(&backup_abs)?;
                let _ = fs::remove_file(&backup_abs);
                out.restored.push(file);
            }
            None => {
                // The file did not exist before lmm; deleting it restores that.
                match fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Error::io(&target, e)),
                }
                out.deleted.push(file);
            }
        }
        ctx.db
            .conn
            .execute("DELETE FROM config_backups WHERE id = ?1", [row_id])?;
    }
    Ok(out)
}

/// Record and store a backup of `path` unless one already exists for this
/// file. Returns true when a new backup was taken.
fn backup_once(
    ctx: &Context,
    inst: &Installation,
    file: &str,
    path: &std::path::Path,
) -> Result<bool> {
    let existing: Option<i64> = ctx
        .db
        .conn
        .query_row(
            "SELECT id FROM config_backups WHERE installation_id = ?1 AND file = ?2",
            params![inst.id, file],
            |r| r.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(false);
    }
    let (backup_rel, sha) = if path.exists() {
        let rel = format!("config/{}/{}", inst.id, file);
        let abs = ctx.paths.backups_dir.join(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).path_ctx(parent)?;
        }
        fs::copy(path, &abs).path_ctx(path)?;
        (Some(rel), Some(sha256_file(path)?))
    } else {
        (None, None)
    };
    ctx.db.conn.execute(
        "INSERT INTO config_backups (installation_id, file, backup_path, sha256, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![inst.id, file, backup_rel, sha, now()],
    )?;
    Ok(true)
}

/// Find `name` in `dir` case-insensitively (Proton prefixes live on
/// case-sensitive filesystems while the game treats names as one).
fn resolve_file(dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    if dir.join(name).exists() {
        return Some(dir.join(name));
    }
    let lower = name.to_lowercase();
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .find_map(|e| (e.file_name().to_string_lossy().to_lowercase() == lower).then(|| e.path()))
}

// ---------------------------------------------------------------------------
// Minimal line-preserving INI editing. Sections are `[Name]`, keys `k=v`;
// both matched case-insensitively, whitespace around keys tolerated.

fn is_section(line: &str, section: &str) -> bool {
    let t = line.trim();
    t.strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|name| name.trim().eq_ignore_ascii_case(section))
}

fn key_of(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.starts_with(';') || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    Some((k.trim(), v.trim()))
}

/// Current value of `[section] key`, if the file has it.
pub fn get_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = is_section(line, section);
            continue;
        }
        if in_section
            && let Some((k, v)) = key_of(line)
            && k.eq_ignore_ascii_case(key)
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Return `text` with `[section] key = value` set, preserving every other
/// line. Missing key: inserted right after the section header. Missing
/// section: appended at the end.
pub fn set_value(text: &str, section: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut section_header: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().starts_with('[') {
            in_section = is_section(line, section);
            if in_section && section_header.is_none() {
                section_header = Some(i);
            }
            continue;
        }
        if in_section
            && let Some((k, _)) = key_of(line)
            && k.eq_ignore_ascii_case(key)
        {
            lines[i] = format!("{key}={value}");
            return finish(lines, text);
        }
    }
    match section_header {
        Some(i) => lines.insert(i + 1, format!("{key}={value}")),
        None => {
            if !lines.is_empty() && !lines.last().is_none_or(|l| l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.push(format!("{key}={value}"));
        }
    }
    finish(lines, text)
}

/// Reassemble lines, keeping the original trailing-newline style.
fn finish(lines: Vec<String>, original: &str) -> String {
    let mut s = lines.join("\n");
    if original.is_empty() || original.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "; comment\n[General]\nsName=Player\n\n[Archive]\nbInvalidateOlderFiles=0\nSInvalidationFile=ArchiveInvalidation.txt\n";

    #[test]
    fn get_and_set_existing_key() {
        assert_eq!(
            get_value(INI, "Archive", "binvalidateolderfiles").as_deref(),
            Some("0")
        );
        let out = set_value(INI, "Archive", "bInvalidateOlderFiles", "1");
        assert_eq!(
            get_value(&out, "Archive", "bInvalidateOlderFiles").as_deref(),
            Some("1")
        );
        // Every other line survives byte-for-byte.
        assert!(out.contains("; comment"));
        assert!(out.contains("sName=Player"));
        assert!(out.contains("SInvalidationFile=ArchiveInvalidation.txt"));
    }

    #[test]
    fn set_empty_value() {
        let out = set_value(INI, "Archive", "SInvalidationFile", "");
        assert_eq!(
            get_value(&out, "Archive", "SInvalidationFile").as_deref(),
            Some("")
        );
    }

    #[test]
    fn insert_missing_key_and_section() {
        let out = set_value(INI, "Archive", "sResourceDataDirsFinal", "");
        assert_eq!(
            get_value(&out, "Archive", "sResourceDataDirsFinal").as_deref(),
            Some("")
        );
        // Inserted inside [Archive], not at EOF: General still first.
        assert!(out.find("[Archive]").unwrap() < out.find("sResourceDataDirsFinal").unwrap());

        let out = set_value(INI, "Launcher", "bEnableFileSelection", "1");
        assert!(out.contains("[Launcher]\nbEnableFileSelection=1"));

        let out = set_value("", "Archive", "bInvalidateOlderFiles", "1");
        assert_eq!(out, "[Archive]\nbInvalidateOlderFiles=1\n");
    }
}
