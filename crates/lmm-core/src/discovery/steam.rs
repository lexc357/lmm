//! Steam game discovery: known Steam roots -> library folders -> app
//! manifests -> installed apps (+ Proton prefixes).
//!
//! Deliberately never walks the filesystem looking for games: only well-known
//! launcher locations and user-configured roots are consulted.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::discovery::vdf;
use crate::error::Result;
use crate::games;

/// An installed Steam app found on disk.
#[derive(Debug, Clone, Serialize)]
pub struct SteamApp {
    pub app_id: u32,
    pub name: String,
    pub install_dir: PathBuf,
    pub library: PathBuf,
    pub proton_prefix: Option<PathBuf>,
    /// Slug in lmm's game registry, if this app is a supported game.
    pub game_slug: Option<String>,
}

/// Well-known Steam root locations on Linux, plus user-configured extras.
/// Only existing directories are returned.
pub fn steam_roots(config: &Config) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/Steam"));
        candidates.push(home.join(".steam/steam"));
        candidates.push(home.join(".steam/root"));
        // Flatpak Steam keeps its own XDG tree under ~/.var.
        candidates.push(home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
    }
    candidates.extend(config.discovery.extra_steam_roots.iter().cloned());

    // ~/.steam/steam and ~/.steam/root are usually symlinks to the main
    // install: canonicalize and dedup so each root is scanned once.
    let mut seen = std::collections::HashSet::new();
    let mut roots = Vec::new();
    for c in candidates {
        if let Ok(canon) = c.canonicalize()
            && canon.join("steamapps").is_dir()
            && seen.insert(canon.clone())
        {
            roots.push(canon);
        }
    }
    roots
}

/// Library folders referenced by a Steam root (always includes the root).
fn libraries(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    let lf = root.join("steamapps/libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&lf)
        && let Ok(doc) = vdf::parse(&text)
        && let Some(folders) = doc.get("libraryfolders")
    {
        for (_idx, folder) in folders.pairs() {
            if let Some(path) = folder.get_str("path") {
                libs.push(PathBuf::from(path));
            }
        }
    }
    libs
}

/// Parse one appmanifest_<id>.acf into a SteamApp, if it is sane.
fn parse_manifest(library: &Path, manifest: &Path) -> Option<SteamApp> {
    let text = fs::read_to_string(manifest).ok()?;
    let doc = vdf::parse(&text).ok()?;
    let state = doc.get("AppState")?;
    let app_id: u32 = state.get_str("appid")?.parse().ok()?;
    let name = state.get_str("name")?.to_string();
    let installdir = state.get_str("installdir")?;
    // installdir is a single directory name; a path separator here would be
    // hostile or corrupt, and we won't follow it.
    if installdir.contains('/') || installdir.contains('\\') || installdir.is_empty() {
        return None;
    }
    let install_dir = library.join("steamapps/common").join(installdir);
    if !install_dir.is_dir() {
        return None; // manifest left over from an uninstalled game
    }
    let pfx = library
        .join("steamapps/compatdata")
        .join(app_id.to_string())
        .join("pfx");
    Some(SteamApp {
        app_id,
        name,
        install_dir,
        library: library.to_path_buf(),
        proton_prefix: pfx.is_dir().then_some(pfx),
        game_slug: games::by_app_id(app_id).map(|g| g.slug.to_string()),
    })
}

/// Discover all installed Steam apps across all roots and libraries.
pub fn discover(config: &Config) -> Result<Vec<SteamApp>> {
    discover_roots(&steam_roots(config))
}

/// Discovery over an explicit set of Steam roots (hermetic; used by tests).
pub fn discover_roots(roots: &[PathBuf]) -> Result<Vec<SteamApp>> {
    // BTreeMap on app_id: dedups apps seen via multiple roots (symlinked or
    // shared libraries) and yields stable, sorted output.
    let mut apps: BTreeMap<u32, SteamApp> = BTreeMap::new();
    let mut seen_libs = std::collections::HashSet::new();

    for root in roots {
        for lib in libraries(root) {
            let Ok(lib) = lib.canonicalize() else {
                continue; // library on an unmounted drive
            };
            if !seen_libs.insert(lib.clone()) {
                continue;
            }
            let steamapps = lib.join("steamapps");
            let Ok(entries) = fs::read_dir(&steamapps) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("appmanifest_")
                    && name.ends_with(".acf")
                    && let Some(app) = parse_manifest(&lib, &entry.path())
                {
                    apps.entry(app.app_id).or_insert(app);
                }
            }
        }
    }
    Ok(apps.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake Steam root with one library and given app manifests.
    fn fake_steam(dir: &Path, apps: &[(u32, &str, &str)]) -> PathBuf {
        let root = dir.join("Steam");
        let steamapps = root.join("steamapps");
        fs::create_dir_all(steamapps.join("common")).unwrap();
        fs::write(
            steamapps.join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                root.display()
            ),
        )
        .unwrap();
        for (id, name, installdir) in apps {
            fs::create_dir_all(steamapps.join("common").join(installdir)).unwrap();
            fs::write(
                steamapps.join(format!("appmanifest_{id}.acf")),
                format!(
                    "\"AppState\"\n{{\n\t\"appid\"\t\"{id}\"\n\t\"name\"\t\"{name}\"\n\t\"installdir\"\t\"{installdir}\"\n}}\n"
                ),
            )
            .unwrap();
        }
        root
    }

    #[test]
    fn discovers_apps_and_flags_supported_games() {
        let dir = tempfile::tempdir().unwrap();
        let root = fake_steam(
            dir.path(),
            &[
                (489830, "Skyrim Special Edition", "Skyrim Special Edition"),
                (440, "Team Fortress 2", "Team Fortress 2"),
            ],
        );
        // Proton prefix for Skyrim only.
        fs::create_dir_all(root.join("steamapps/compatdata/489830/pfx")).unwrap();

        let apps = discover_roots(&[root]).unwrap();
        assert_eq!(apps.len(), 2);
        let skyrim = apps.iter().find(|a| a.app_id == 489830).unwrap();
        assert_eq!(skyrim.game_slug.as_deref(), Some("skyrimse"));
        assert!(skyrim.proton_prefix.is_some());
        let tf2 = apps.iter().find(|a| a.app_id == 440).unwrap();
        assert_eq!(tf2.game_slug, None);
        assert_eq!(tf2.proton_prefix, None);
    }

    #[test]
    fn skips_manifest_without_install_dir_and_hostile_installdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = fake_steam(dir.path(), &[(100, "Gone", "GoneDir")]);
        fs::remove_dir(root.join("steamapps/common/GoneDir")).unwrap();
        // Hostile installdir trying to point outside the library.
        fs::write(
            root.join("steamapps/appmanifest_101.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\"101\"\n\t\"name\"\t\"Evil\"\n\t\"installdir\"\t\"../../../etc\"\n}\n",
        )
        .unwrap();
        let apps = discover_roots(&[root]).unwrap();
        assert!(apps.is_empty());
    }
}
