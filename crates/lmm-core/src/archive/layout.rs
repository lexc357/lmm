//! Mod-archive layout detection: find the directory inside an extracted tree
//! that corresponds to the game's mod root.
//!
//! Mod authors package the same content many ways: `Data/textures/...`,
//! `textures/...`, `MyMod-1.2/Data/...`. We pick the root with simple,
//! explainable rules; when nothing matches we install as-is and say so, so
//! the user can inspect with `--dry-run` before deploying.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};
use crate::games::{GameDef, Layout};

/// File extensions that mark a directory as Bethesda `Data/` content.
const DATA_FILE_EXTS: &[&str] = &["esp", "esm", "esl", "bsa", "ba2"];
/// Directory names that mark a directory as Bethesda `Data/` content.
const DATA_DIR_NAMES: &[&str] = &[
    "meshes",
    "textures",
    "scripts",
    "interface",
    "sound",
    "music",
    "strings",
    "video",
    "seq",
    "grass",
    "shadersfx",
    "skse",
    "mcm",
    "dialogueviews",
];
/// Files ignored when judging what a directory "contains".
fn is_incidental(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".txt")
        || lower.ends_with(".md")
        || lower.ends_with(".pdf")
        || lower.ends_with(".url")
        || lower.starts_with("readme")
        || lower.starts_with("fomod") // installer metadata, not game content
}

#[derive(Debug)]
pub struct DetectedLayout {
    /// Directory within the extracted tree whose contents map onto the
    /// game's mod root.
    pub root: PathBuf,
    /// Human-readable rule that fired, for verbose/dry-run output.
    pub rule: &'static str,
    /// True when we fell back to "install as-is" without recognizing content.
    pub uncertain: bool,
}

pub fn detect_mod_root(extracted: &Path, game: &GameDef) -> Result<DetectedLayout> {
    match game.layout {
        Layout::GameRoot => {
            let (root, descended) = descend_single_wrappers(extracted)?;
            Ok(DetectedLayout {
                root,
                rule: if descended {
                    "unwrapped single top-level directory"
                } else {
                    "archive root taken as-is"
                },
                uncertain: false,
            })
        }
        Layout::BethesdaData => detect_bethesda(extracted, game),
        Layout::ModFolder => detect_mod_folder(extracted),
    }
}

fn detect_bethesda(extracted: &Path, game: &GameDef) -> Result<DetectedLayout> {
    // "Data" for most games, "Data Files" for Morrowind.
    let data_name = game.mod_root.to_lowercase();
    let mut current = extracted.to_path_buf();
    // Bounded descent: wrapper dirs are one or two levels in practice.
    for _ in 0..4 {
        // Rule 1: an explicit data directory (any casing) wins.
        if let Some(data_dir) = find_dir_ci(&current, &data_name)? {
            return Ok(DetectedLayout {
                root: data_dir,
                rule: "found the game's data directory in the archive",
                uncertain: false,
            });
        }
        // Rule 2: the directory itself looks like Data/ content.
        if has_data_markers(&current)? {
            return Ok(DetectedLayout {
                root: current,
                rule: "archive root is Data/ content (plugins or asset directories)",
                uncertain: false,
            });
        }
        // Rule 3: single wrapper directory -> descend and retry.
        if let Some(only) = single_subdir(&current)? {
            current = only;
            continue;
        }
        break;
    }
    Ok(DetectedLayout {
        root: extracted.to_path_buf(),
        rule: "no recognized layout; installing archive root as Data/ content",
        uncertain: true,
    })
}

/// ModFolder games (Stardew `Mods/`, Bannerlord `Modules/`): every top-level
/// directory is a mod folder whose name is meaningful, so nothing is ever
/// unwrapped. Loose files at the root mean the author forgot the folder —
/// installing as-is would spill them into the mod root, so that is flagged.
fn detect_mod_folder(extracted: &Path) -> Result<DetectedLayout> {
    let mut has_dir = false;
    let mut has_loose_file = false;
    for entry in fs::read_dir(extracted).path_ctx(extracted)? {
        let entry = entry.path_ctx(extracted)?;
        if entry.file_type().path_ctx(extracted)?.is_dir() {
            has_dir = true;
        } else if !is_incidental(&entry.file_name().to_string_lossy()) {
            has_loose_file = true;
        }
    }
    Ok(DetectedLayout {
        root: extracted.to_path_buf(),
        rule: if has_dir && !has_loose_file {
            "top-level mod folder(s) taken as-is"
        } else {
            "archive has loose files at its root; they would land directly \
             in the mod directory"
        },
        uncertain: !has_dir || has_loose_file,
    })
}

/// Direct subdirectory with the given lowercase name, if present.
fn find_dir_ci(dir: &Path, name_lower: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir).path_ctx(dir)? {
        let entry = entry.path_ctx(dir)?;
        if entry.file_type().path_ctx(dir)?.is_dir()
            && entry.file_name().to_string_lossy().to_lowercase() == name_lower
        {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn has_data_markers(dir: &Path) -> Result<bool> {
    for entry in fs::read_dir(dir).path_ctx(dir)? {
        let entry = entry.path_ctx(dir)?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let ft = entry.file_type().path_ctx(dir)?;
        if ft.is_dir() && DATA_DIR_NAMES.contains(&name.as_str()) {
            return Ok(true);
        }
        if ft.is_file()
            && let Some(ext) = name.rsplit_once('.').map(|(_, e)| e)
            && DATA_FILE_EXTS.contains(&ext)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// If `dir` contains exactly one subdirectory and no significant files,
/// return that subdirectory.
fn single_subdir(dir: &Path) -> Result<Option<PathBuf>> {
    let mut only_dir: Option<PathBuf> = None;
    for entry in fs::read_dir(dir).path_ctx(dir)? {
        let entry = entry.path_ctx(dir)?;
        let ft = entry.file_type().path_ctx(dir)?;
        if ft.is_dir() {
            if only_dir.replace(entry.path()).is_some() {
                return Ok(None); // more than one directory
            }
        } else if !is_incidental(&entry.file_name().to_string_lossy()) {
            return Ok(None); // significant file at this level
        }
    }
    Ok(only_dir)
}

/// For GameRoot layouts: unwrap at most one wrapper directory. Deeper
/// descent would eat real directory structure ("MyMod-3.0/bin/mod.dll"
/// must deploy as "bin/mod.dll", not "mod.dll").
fn descend_single_wrappers(extracted: &Path) -> Result<(PathBuf, bool)> {
    match single_subdir(extracted)? {
        Some(only) => Ok((only, true)),
        None => Ok((extracted.to_path_buf(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games;

    fn skyrim() -> &'static GameDef {
        games::by_slug("skyrimse").unwrap()
    }

    fn mk(dir: &Path, files: &[&str]) {
        for f in files {
            let p = dir.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        }
    }

    #[test]
    fn explicit_data_dir() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["Data/MyMod.esp", "readme.txt"]);
        let d = detect_mod_root(t.path(), skyrim()).unwrap();
        assert_eq!(d.root, t.path().join("Data"));
        assert!(!d.uncertain);
    }

    #[test]
    fn loose_data_content_at_root() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["MyMod.esp", "textures/armor/a.dds"]);
        let d = detect_mod_root(t.path(), skyrim()).unwrap();
        assert_eq!(d.root, t.path());
    }

    #[test]
    fn wrapper_dir_then_data() {
        let t = tempfile::tempdir().unwrap();
        mk(
            t.path(),
            &[
                "MyMod-1.2/Data/MyMod.esp",
                "MyMod-1.2/readme.txt",
                "README.md",
            ],
        );
        let d = detect_mod_root(t.path(), skyrim()).unwrap();
        assert_eq!(d.root, t.path().join("MyMod-1.2/Data"));
    }

    #[test]
    fn wrapper_dir_with_loose_content() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["MyMod/meshes/a.nif", "MyMod/MyMod.esp"]);
        let d = detect_mod_root(t.path(), skyrim()).unwrap();
        assert_eq!(d.root, t.path().join("MyMod"));
    }

    #[test]
    fn unrecognized_falls_back_uncertain() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["weird/stuff.bin", "other/thing.dat"]);
        let d = detect_mod_root(t.path(), skyrim()).unwrap();
        assert_eq!(d.root, t.path());
        assert!(d.uncertain);
    }

    #[test]
    fn morrowind_data_files_dir() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["Data Files/MyMod.esp", "readme.txt"]);
        let mw = games::by_slug("morrowind").unwrap();
        let d = detect_mod_root(t.path(), mw).unwrap();
        assert_eq!(d.root, t.path().join("Data Files"));
        assert!(!d.uncertain);
    }

    #[test]
    fn mod_folder_keeps_top_level_dirs() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["SomeMod/manifest.json", "readme.txt"]);
        let sdv = games::by_slug("stardewvalley").unwrap();
        let d = detect_mod_root(t.path(), sdv).unwrap();
        assert_eq!(d.root, t.path());
        assert!(!d.uncertain, "the wrapper is the mod folder, never unwrap");
    }

    #[test]
    fn mod_folder_loose_files_flagged() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["manifest.json", "mod.dll"]);
        let sdv = games::by_slug("stardewvalley").unwrap();
        let d = detect_mod_root(t.path(), sdv).unwrap();
        assert!(d.uncertain);
    }

    #[test]
    fn generic_game_unwraps_wrapper() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["MyMod-3.0/bin/mod.dll", "readme.txt"]);
        let generic = games::by_slug("generic").unwrap();
        let d = detect_mod_root(t.path(), generic).unwrap();
        assert_eq!(d.root, t.path().join("MyMod-3.0"));
    }
}
