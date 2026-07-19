//! Game Tools: per-game modding utilities and one-time setup tasks.
//!
//! This module is the engine behind the CLI's `tools` section: a built-in
//! catalog of community-standard tools per game ([`registry`]), an install
//! engine with a full file manifest and displaced-file backups ([`install`]),
//! INI configuration tweaks ([`gameconfig`]), plugin load-order maintenance
//! ([`loadorder`]) and a composed health check ([`health`]).
//!
//! Like the rest of lmm-core, nothing here prints or prompts; destructive
//! operations are explicit and everything returns structured data.

pub mod gameconfig;
pub mod health;
pub mod install;
pub mod loadorder;
pub mod registry;

use std::path::PathBuf;

use serde::Serialize;

use crate::Context;
use crate::config::DataPaths;
use crate::error::{Error, Result};
use crate::games;
use crate::model::Installation;

use registry::{GameTools, Target, ToolDef, ToolKind};

/// The catalog for an installation's game, or a helpful error.
pub fn catalog(inst: &Installation) -> Result<&'static GameTools> {
    registry::for_game(&inst.game_slug).ok_or_else(|| {
        Error::Invalid(format!(
            "no tool catalog for '{}'; Game Tools knows the Bethesda family, \
             Stardew Valley and Cyberpunk 2077",
            inst.game_slug
        ))
    })
}

/// Resolve a tool selector against an installation's catalog.
pub fn find_tool(inst: &Installation, selector: &str) -> Result<&'static ToolDef> {
    let game = catalog(inst)?;
    registry::find_tool(game, selector).ok_or_else(|| {
        Error::NotFound(format!(
            "tool '{selector}' (known tools for {}: {})",
            inst.game_slug,
            game.tools
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// Directory a tool's files are measured against and installed into.
pub fn target_root(paths: &DataPaths, inst: &Installation, tool: &ToolDef) -> PathBuf {
    match tool.target {
        Target::GameRoot => inst.path.clone(),
        Target::ModRoot => {
            let mod_root = games::by_slug(&inst.game_slug)
                .map(|g| g.mod_root)
                .unwrap_or("");
            if mod_root.is_empty() {
                inst.path.clone()
            } else {
                inst.path.join(mod_root)
            }
        }
        Target::Standalone => paths.tools_dir.join(inst.id.to_string()).join(tool.id),
    }
}

/// A tool's state as shown in the Game Tools listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolState {
    Installed,
    Missing,
    Outdated,
    /// Something is off: files missing or only partially present.
    Attention,
}

impl ToolState {
    pub fn describe(self) -> &'static str {
        match self {
            ToolState::Installed => "installed",
            ToolState::Missing => "missing",
            ToolState::Outdated => "outdated",
            ToolState::Attention => "attention",
        }
    }
}

/// One row of the Game Tools listing.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub id: String,
    pub name: String,
    pub kind: ToolKind,
    pub tier: registry::Tier,
    pub state: ToolState,
    /// True when lmm installed it (so update/verify/remove work on it).
    pub managed: bool,
    pub version: Option<String>,
    pub latest_known: Option<String>,
    /// Extra context for the state ("found on disk", "2 files missing", ...).
    pub detail: Option<String>,
    pub url: String,
    /// Nexus (domain, mod id) if downloadable from Nexus Mods.
    pub nexus: Option<(String, u32)>,
}

/// Status of every tool in the installation's catalog.
pub fn status(ctx: &Context, inst: &Installation) -> Result<Vec<ToolStatus>> {
    let game = catalog(inst)?;
    game.tools
        .iter()
        .map(|t| tool_status(ctx, inst, t))
        .collect()
}

/// Status of one tool: managed manifest first, then on-disk detection.
pub fn tool_status(ctx: &Context, inst: &Installation, tool: &ToolDef) -> Result<ToolStatus> {
    let mut st = ToolStatus {
        id: tool.id.to_string(),
        name: tool.name.to_string(),
        kind: tool.kind,
        tier: tool.tier,
        state: ToolState::Missing,
        managed: false,
        version: None,
        latest_known: tool.latest_known.map(str::to_string),
        detail: None,
        url: tool.url.to_string(),
        nexus: tool.nexus.map(|(d, id)| (d.to_string(), id)),
    };

    if let Some(rec) = install::get_record(&ctx.db, inst.id, tool.id)? {
        st.managed = true;
        st.version = rec.version.clone();
        let root = target_root(&ctx.paths, inst, tool);
        // Listing checks existence only; `tools verify` re-hashes content.
        let missing = install::files(&ctx.db, rec.row_id)?
            .iter()
            .filter(|f| !f.rel.to_native(&root).exists())
            .count();
        if missing > 0 {
            st.state = ToolState::Attention;
            st.detail = Some(format!(
                "{missing} installed file(s) missing on disk; run 'tools verify {}'",
                tool.id
            ));
        } else if is_outdated(rec.version.as_deref(), tool.latest_known) {
            st.state = ToolState::Outdated;
            st.detail = Some(format!(
                "version {} installed, {} available",
                rec.version.as_deref().unwrap_or("?"),
                tool.latest_known.unwrap_or("?"),
            ));
        } else {
            st.state = ToolState::Installed;
        }
        return Ok(st);
    }

    // Not managed: probe the detect paths for an external installation.
    let root = target_root(&ctx.paths, inst, tool);
    let found = tool
        .detect
        .iter()
        .filter(|pat| detect_match(&root, pat))
        .count();
    if !tool.detect.is_empty() && found == tool.detect.len() {
        st.state = ToolState::Installed;
        st.detail = Some("found on disk (not installed via lmm)".into());
    } else if found > 0 {
        st.state = ToolState::Attention;
        st.detail = Some(format!(
            "only {found} of {} expected file(s) present",
            tool.detect.len()
        ));
    } else if tool.target == Target::Standalone && root.is_dir() {
        // A leftover standalone dir without a manifest still counts as found.
        st.state = ToolState::Installed;
        st.detail = Some("directory present (not recorded by lmm)".into());
    }
    Ok(st)
}

/// Does `pattern` (relative, optional `*` in the final component) match an
/// existing file under `root`? Also anchors the installer's wrapper-dir
/// unwrapping.
pub(crate) fn detect_match(root: &std::path::Path, pattern: &str) -> bool {
    let (dir, last) = pattern.rsplit_once('/').unwrap_or(("", pattern));
    let dir_path = if dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir)
    };
    match last.split_once('*') {
        None => dir_path.join(last).exists(),
        Some((prefix, suffix)) => std::fs::read_dir(&dir_path)
            .map(|entries| {
                entries.flatten().any(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(prefix) && name.ends_with(suffix)
                })
            })
            .unwrap_or(false),
    }
}

/// Lenient numeric version comparison: "2.2.3" < "2.2.6", "0.7.2" == "v0.7.2".
/// Unknown installed versions are never reported outdated.
fn is_outdated(installed: Option<&str>, latest: Option<&str>) -> bool {
    let (Some(installed), Some(latest)) = (installed, latest) else {
        return false;
    };
    let nums = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    let (a, b) = (nums(installed), nums(latest));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a < b
}

/// How to start a tool. The frontend performs the spawn (a session side
/// effect, like printing).
#[derive(Debug, Serialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum ToolLaunch {
    /// Native Linux executable: run it directly.
    Native { exe: PathBuf, cwd: PathBuf },
    /// Windows executable: run through Proton against the game's prefix.
    Proton {
        proton: PathBuf,
        exe: PathBuf,
        cwd: PathBuf,
        /// steamapps/compatdata/<appid> — STEAM_COMPAT_DATA_PATH.
        compat_data: PathBuf,
        /// Steam client root — STEAM_COMPAT_CLIENT_INSTALL_PATH.
        steam_root: PathBuf,
    },
}

/// Work out how to launch a tool, or explain why it cannot be launched.
pub fn launch_method(ctx: &Context, inst: &Installation, tool: &ToolDef) -> Result<ToolLaunch> {
    if tool.kind == ToolKind::ScriptExtender {
        return Err(Error::Invalid(format!(
            "{} runs instead of the game, not as a separate program: in Steam set the game's \
             launch options to \"path/to/loader.exe %command%\" (or launch the loader from \
             the game directory), then start the game normally",
            tool.name
        )));
    }
    let Some(exe_rel) = tool.exe else {
        return Err(Error::Invalid(format!(
            "{} has no launchable executable; it loads with the game",
            tool.name
        )));
    };
    let root = target_root(&ctx.paths, inst, tool);
    let exe = root.join(exe_rel);
    if !exe.is_file() {
        return Err(Error::NotFound(format!(
            "{} not found; install the tool first ('tools install {}')",
            exe.display(),
            tool.id
        )));
    }
    let cwd = exe.parent().map(PathBuf::from).unwrap_or(root);

    if !exe
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Ok(ToolLaunch::Native { exe, cwd });
    }

    // Windows tool: needs the game's Proton prefix and an installed Proton.
    let prefix = inst.proton_prefix.clone().ok_or_else(|| {
        Error::Invalid(format!(
            "{} is a Windows program and this installation has no Proton prefix; \
             run the game once through Steam first",
            tool.name
        ))
    })?;
    let compat_data = prefix
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Invalid("malformed proton prefix path".into()))?;

    let apps = crate::discovery::steam::discover(&ctx.config)?;
    let proton = apps
        .iter()
        .filter(|a| a.name.starts_with("Proton") && a.install_dir.join("proton").is_file())
        .max_by(|a, b| proton_rank(&a.name).cmp(&proton_rank(&b.name)))
        .map(|a| a.install_dir.join("proton"))
        .ok_or_else(|| {
            Error::NotFound(
                "no Proton installation found in any Steam library; install one via Steam \
                 (Library -> Tools) to launch Windows tools"
                    .into(),
            )
        })?;
    let steam_root = crate::discovery::steam::steam_roots(&ctx.config)
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound("no Steam root found".into()))?;

    Ok(ToolLaunch::Proton {
        proton,
        exe,
        cwd,
        compat_data,
        steam_root,
    })
}

/// Order Proton installs: Experimental beats numbered releases, higher
/// version numbers beat lower ones.
fn proton_rank(name: &str) -> (u8, Vec<u64>) {
    let experimental = u8::from(name.contains("Experimental"));
    let nums = name
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    (experimental, nums)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_lenient() {
        assert!(is_outdated(Some("2.2.3"), Some("2.2.6")));
        assert!(is_outdated(Some("v0.6.23"), Some("0.7.2")));
        assert!(!is_outdated(Some("2.2.6"), Some("2.2.6")));
        assert!(!is_outdated(Some("2.3"), Some("2.2.6")));
        assert!(!is_outdated(None, Some("1.0")));
        assert!(!is_outdated(Some("1.0"), None));
        assert!(!is_outdated(Some("beta"), Some("1.0")));
    }

    #[test]
    fn proton_ordering() {
        assert!(proton_rank("Proton - Experimental") > proton_rank("Proton 9.0 (Beta)"));
        assert!(proton_rank("Proton 9.0") > proton_rank("Proton 8.0"));
        assert!(proton_rank("Proton 10.0") > proton_rank("Proton 9.0"));
    }

    #[test]
    fn detect_match_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("SKSE/Plugins")).unwrap();
        std::fs::write(root.join("SKSE/Plugins/version-1-6-1170-0.bin"), b"x").unwrap();
        assert!(detect_match(root, "SKSE/Plugins/version-*.bin"));
        assert!(!detect_match(root, "SKSE/Plugins/version-*.csv"));
        assert!(!detect_match(root, "skse64_loader.exe"));
        std::fs::write(root.join("skse64_loader.exe"), b"x").unwrap();
        assert!(detect_match(root, "skse64_loader.exe"));
    }
}
