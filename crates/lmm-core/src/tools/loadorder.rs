//! Plugin load-order maintenance for Bethesda-family games: analyze
//! `plugins.txt`, sort it with best-practice rules, and restore earlier
//! versions from automatic backups.
//!
//! The sorter is deliberately conservative — official plugins first in
//! their fixed order, then masters, then regular plugins, with each
//! plugin guaranteed to load after its masters and ties broken by the
//! current order (a stable topological sort). It fixes the errors that
//! break games; nuanced inter-mod ordering stays LOOT's job, which the
//! tool catalog offers right next to this.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Context;
use crate::db::now;
use crate::error::{Error, IoContext, Result};
use crate::games;
use crate::model::Installation;
use crate::tools::registry::{GameTools, PluginsSpec};

/// Extensions the engine treats as plugins.
const PLUGIN_EXTS: &[&str] = &["esm", "esp", "esl"];

/// Path of the game's `plugins.txt` (inside the Proton prefix).
pub fn plugins_path(inst: &Installation, spec: &PluginsSpec) -> Result<PathBuf> {
    let prefix = inst.proton_prefix.as_ref().ok_or_else(|| {
        Error::Invalid(
            "this installation has no Proton prefix; run the game once through Steam \
             so it creates its plugin list"
                .into(),
        )
    })?;
    Ok(prefix
        .join("drive_c/users/steamuser/AppData/Local")
        .join(spec.local_dir)
        .join("plugins.txt"))
}

fn spec_of(inst: &Installation, game: &GameTools) -> Result<PluginsSpec> {
    game.plugins.ok_or_else(|| {
        Error::Invalid(format!(
            "'{}' does not use a plugins.txt load order",
            inst.game_slug
        ))
    })
}

/// One line of plugins.txt, in file order.
#[derive(Debug, Clone, Serialize)]
pub struct PluginEntry {
    pub name: String,
    pub enabled: bool,
}

/// Parsed header facts about a plugin file on disk.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PluginMeta {
    pub is_master: bool,
    pub is_light: bool,
    pub masters: Vec<String>,
}

/// A listed plugin joined with what is actually in the Data directory.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub enabled: bool,
    pub present: bool,
    pub official: bool,
    #[serde(flatten)]
    pub meta: PluginMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueKind {
    /// Listed in plugins.txt but the file is gone from Data.
    MissingFile,
    /// A required master is not in the Data directory at all.
    MissingMaster,
    /// A required master exists but is listed after (or disabled).
    MasterOutOfOrder,
    /// The same plugin appears more than once.
    Duplicate,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub kind: IssueKind,
    pub plugin: String,
    pub detail: String,
}

/// Everything `tools loadorder` shows.
#[derive(Debug, Serialize)]
pub struct Analysis {
    pub path: PathBuf,
    pub plugins: Vec<PluginInfo>,
    /// Plugin files in Data that plugins.txt does not mention.
    pub unlisted: Vec<String>,
    pub issues: Vec<Issue>,
    /// Older games also honor file timestamps; sorting plugins.txt alone
    /// may not be the full story there.
    pub timestamp_caveat: bool,
}

/// Read and analyze the current plugin list against the Data directory.
pub fn analyze(inst: &Installation, game: &GameTools) -> Result<Analysis> {
    let spec = spec_of(inst, game)?;
    let path = plugins_path(inst, &spec)?;
    let entries = read_plugins(&path, &spec)?;
    let data_dir = data_dir(inst);
    let on_disk = scan_data_dir(&data_dir)?;
    let on_disk_lower: HashMap<String, String> = on_disk
        .iter()
        .map(|n| (n.to_lowercase(), n.clone()))
        .collect();

    let mut issues = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut plugins = Vec::new();
    for e in &entries {
        if !seen.insert(e.name.to_lowercase()) {
            issues.push(Issue {
                kind: IssueKind::Duplicate,
                plugin: e.name.clone(),
                detail: "listed more than once".into(),
            });
            continue;
        }
        // Resolve against the on-disk casing: plugins.txt may disagree with
        // the filesystem, which is case-sensitive under Linux.
        let actual = on_disk_lower.get(&e.name.to_lowercase());
        let meta = if let Some(actual) = actual {
            plugin_meta(&data_dir.join(actual)).unwrap_or_default()
        } else {
            issues.push(Issue {
                kind: IssueKind::MissingFile,
                plugin: e.name.clone(),
                detail: "listed in plugins.txt but not in the Data directory".into(),
            });
            PluginMeta::default()
        };
        plugins.push(PluginInfo {
            official: is_official(&spec, &e.name),
            name: e.name.clone(),
            enabled: e.enabled,
            present: actual.is_some(),
            meta,
        });
    }

    // Master checks, against listed positions.
    let position: HashMap<String, usize> = plugins
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.to_lowercase(), i))
        .collect();
    for (i, p) in plugins.iter().enumerate() {
        if !p.enabled {
            continue;
        }
        for m in &p.meta.masters {
            let key = m.to_lowercase();
            if !on_disk_lower.contains_key(&key) {
                issues.push(Issue {
                    kind: IssueKind::MissingMaster,
                    plugin: p.name.clone(),
                    detail: format!("requires '{m}', which is not installed"),
                });
            } else {
                match position.get(&key) {
                    Some(&mi) if mi < i && plugins[mi].enabled => {}
                    Some(&mi) if !plugins[mi].enabled => issues.push(Issue {
                        kind: IssueKind::MasterOutOfOrder,
                        plugin: p.name.clone(),
                        detail: format!("requires '{m}', which is disabled"),
                    }),
                    Some(_) => issues.push(Issue {
                        kind: IssueKind::MasterOutOfOrder,
                        plugin: p.name.clone(),
                        detail: format!("loads before its master '{m}'"),
                    }),
                    None => issues.push(Issue {
                        kind: IssueKind::MasterOutOfOrder,
                        plugin: p.name.clone(),
                        detail: format!("requires '{m}', which is not in plugins.txt"),
                    }),
                }
            }
        }
    }

    let listed: HashSet<String> = plugins.iter().map(|p| p.name.to_lowercase()).collect();
    let mut unlisted: Vec<String> = on_disk
        .iter()
        .filter(|n| !listed.contains(&n.to_lowercase()))
        .cloned()
        .collect();
    unlisted.sort();

    Ok(Analysis {
        path,
        plugins,
        unlisted,
        issues,
        timestamp_caveat: !spec.asterisk,
    })
}

/// The proposed order, next to what is currently on disk.
#[derive(Debug, Serialize)]
pub struct SortPlan {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub changed: bool,
}

/// Stable best-practice sort of the analyzed list (see module docs).
pub fn plan_sort(analysis: &Analysis, game: &GameTools) -> SortPlan {
    let spec = game
        .plugins
        .as_ref()
        .expect("analysis implies plugins spec");
    let plugins = &analysis.plugins;

    // Sort key per plugin: official rank, then master-ness, then current
    // position — the tie-break that makes the topological sort stable.
    let key = |i: usize| -> (u8, usize, usize) {
        let p = &plugins[i];
        let official_pos = spec
            .official
            .iter()
            .position(|o| o.eq_ignore_ascii_case(&p.name));
        match official_pos {
            Some(pos) => (0, pos, i),
            None if p.meta.is_master || p.name.to_lowercase().ends_with(".esm") => (1, 0, i),
            None => (2, 0, i),
        }
    };

    let index_of: HashMap<String, usize> = plugins
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.to_lowercase(), i))
        .collect();

    // Edges: master -> dependent, only between listed plugins.
    let mut indegree = vec![0usize; plugins.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); plugins.len()];
    for (i, p) in plugins.iter().enumerate() {
        for m in &p.meta.masters {
            if let Some(&mi) = index_of.get(&m.to_lowercase())
                && mi != i
            {
                dependents[mi].push(i);
                indegree[i] += 1;
            }
        }
    }

    // Kahn's algorithm, always taking the smallest key among ready nodes.
    let mut ready: Vec<usize> = (0..plugins.len()).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(plugins.len());
    let mut done = vec![false; plugins.len()];
    while order.len() < plugins.len() {
        let next = match ready.iter().copied().min_by_key(|&i| key(i)) {
            Some(n) => n,
            // Dependency cycle (malformed plugins): fall back to the
            // smallest-key unfinished node so the sort still terminates.
            None => (0..plugins.len())
                .filter(|&i| !done[i])
                .min_by_key(|&i| key(i))
                .expect("unfinished node exists"),
        };
        ready.retain(|&i| i != next);
        done[next] = true;
        order.push(next);
        for &d in &dependents[next] {
            if !done[d] {
                indegree[d] -= 1;
                if indegree[d] == 0 {
                    ready.push(d);
                }
            }
        }
    }

    let before: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
    let after: Vec<String> = order.iter().map(|&i| plugins[i].name.clone()).collect();
    SortPlan {
        changed: before != after,
        before,
        after,
    }
}

/// Write the sorted order back, after backing up the current file.
/// Returns the backup path.
pub fn apply_sort(
    ctx: &Context,
    inst: &Installation,
    game: &GameTools,
    analysis: &Analysis,
    plan: &SortPlan,
) -> Result<PathBuf> {
    let spec = spec_of(inst, game)?;
    let backup = backup_current(ctx, inst, &analysis.path)?;
    let enabled: HashMap<String, bool> = analysis
        .plugins
        .iter()
        .map(|p| (p.name.clone(), p.enabled))
        .collect();
    let mut text = String::new();
    for name in &plan.after {
        let on = enabled.get(name).copied().unwrap_or(true);
        if spec.asterisk {
            if on {
                text.push('*');
            }
            text.push_str(name);
            text.push('\n');
        } else if on {
            // Older format lists only enabled plugins.
            text.push_str(name);
            text.push('\n');
        }
    }
    let tmp = analysis.path.with_extension("lmm-tmp");
    fs::write(&tmp, &text).path_ctx(&tmp)?;
    fs::rename(&tmp, &analysis.path).path_ctx(&analysis.path)?;
    Ok(backup)
}

/// Load-order backups for an installation, newest first.
pub fn backups(ctx: &Context, inst: &Installation) -> Result<Vec<PathBuf>> {
    let dir = ctx.paths.loadorder_dir.join(inst.id.to_string());
    let mut out = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "txt") {
                    out.push(e.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(&dir, e)),
    }
    out.sort();
    out.reverse();
    Ok(out)
}

/// Restore a backup (the newest, or a specific file from [`backups`]).
/// The current plugins.txt is backed up first, so restores are undoable.
pub fn restore(
    ctx: &Context,
    inst: &Installation,
    game: &GameTools,
    which: Option<&Path>,
) -> Result<PathBuf> {
    let spec = spec_of(inst, game)?;
    let path = plugins_path(inst, &spec)?;
    let backup = match which {
        Some(p) => p.to_path_buf(),
        None => backups(ctx, inst)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound("no load-order backups recorded".into()))?,
    };
    if !backup.exists() {
        return Err(Error::NotFound(format!("backup {}", backup.display())));
    }
    if path.exists() {
        backup_current(ctx, inst, &path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).path_ctx(parent)?;
    }
    fs::copy(&backup, &path).path_ctx(&backup)?;
    Ok(backup)
}

fn backup_current(ctx: &Context, inst: &Installation, path: &Path) -> Result<PathBuf> {
    let dir = ctx.paths.loadorder_dir.join(inst.id.to_string());
    fs::create_dir_all(&dir).path_ctx(&dir)?;
    // Millisecond stamp, zero-padded so lexicographic order = age, bumped
    // until free so two backups in the same instant never overwrite.
    let mut ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_else(|_| now() as u128 * 1000);
    loop {
        let backup = dir.join(format!("plugins-{ts:013}.txt"));
        if !backup.exists() {
            fs::copy(path, &backup).path_ctx(path)?;
            return Ok(backup);
        }
        ts += 1;
    }
}

fn data_dir(inst: &Installation) -> PathBuf {
    let mod_root = games::by_slug(&inst.game_slug)
        .map(|g| g.mod_root)
        .unwrap_or("");
    if mod_root.is_empty() {
        inst.path.clone()
    } else {
        inst.path.join(mod_root)
    }
}

fn is_official(spec: &PluginsSpec, name: &str) -> bool {
    spec.official.iter().any(|o| o.eq_ignore_ascii_case(name))
}

/// Parse plugins.txt. A missing file is an empty list, not an error — the
/// game may simply never have run.
pub fn read_plugins(path: &Path, spec: &PluginsSpec) -> Result<Vec<PluginEntry>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(path, e)),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, enabled) = if spec.asterisk {
            match line.strip_prefix('*') {
                Some(rest) => (rest.trim(), true),
                None => (line, false),
            }
        } else {
            (line, true)
        };
        if !name.is_empty() {
            out.push(PluginEntry {
                name: name.to_string(),
                enabled,
            });
        }
    }
    Ok(out)
}

/// Plugin files in the Data directory (top level only, like the engine).
fn scan_data_dir(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::io(dir, e)),
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ext = name.rsplit_once('.').map(|(_, x)| x.to_lowercase());
        if ext.is_some_and(|x| PLUGIN_EXTS.contains(&x.as_str())) && e.path().is_file() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Plugin header parsing (TES4 format: Oblivion through Starfield).
//
// Layout: record header ("TES4", dataSize, flags, formid, vc[, version,
// unknown]) followed by subrecords (tag[4], size u16, data). Oblivion-era
// headers are 20 bytes, Skyrim-era 24 — told apart by where the first
// subrecord tag ("HEDR") lands. Masters are the MAST subrecords.

const MASTER_FLAG: u32 = 0x1;
const LIGHT_FLAG: u32 = 0x200;
/// Sanity cap on the header record we are willing to read.
const MAX_HEADER: usize = 8 * 1024 * 1024;

pub fn plugin_meta(path: &Path) -> Result<PluginMeta> {
    use std::io::Read;
    let mut f = fs::File::open(path).path_ctx(path)?;
    let mut head = [0u8; 28];
    let n = f.read(&mut head).path_ctx(path)?;
    if n < 28 || &head[0..4] != b"TES4" {
        // Not a TES4-family plugin (TES3, corrupt, or too short): fall back
        // to extension-derived facts so analysis can continue.
        let name = path.file_name().map(|s| s.to_string_lossy().to_lowercase());
        return Ok(PluginMeta {
            is_master: name.as_deref().is_some_and(|n| n.ends_with(".esm")),
            is_light: name.as_deref().is_some_and(|n| n.ends_with(".esl")),
            masters: Vec::new(),
        });
    }
    let data_size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let flags = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let header_len = if &head[20..24] == b"HEDR" { 20 } else { 24 };

    let is_light = flags & LIGHT_FLAG != 0
        || path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("esl"));
    let mut meta = PluginMeta {
        is_master: flags & MASTER_FLAG != 0,
        is_light,
        masters: Vec::new(),
    };

    let to_read = data_size.min(MAX_HEADER);
    // We already consumed 28 bytes; subrecords start at header_len.
    let mut buf = Vec::with_capacity(28 - header_len + to_read);
    buf.extend_from_slice(&head[header_len..]);
    let mut rest = vec![0u8; to_read.saturating_sub(buf.len())];
    let got = read_up_to(&mut f, &mut rest).path_ctx(path)?;
    buf.extend_from_slice(&rest[..got]);

    // Walk subrecords; XXXX overrides the next subrecord's 16-bit size.
    let mut pos = 0usize;
    let mut oversize: Option<usize> = None;
    while pos + 6 <= buf.len() {
        let tag = &buf[pos..pos + 4];
        let size16 = u16::from_le_bytes([buf[pos + 4], buf[pos + 5]]) as usize;
        pos += 6;
        let size = oversize.take().unwrap_or(size16);
        if pos + size > buf.len() {
            break;
        }
        match tag {
            b"XXXX" if size == 4 => {
                oversize =
                    Some(
                        u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
                            as usize,
                    );
            }
            b"MAST" => {
                let raw = &buf[pos..pos + size];
                let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
                meta.masters.push(String::from_utf8_lossy(raw).into_owned());
            }
            _ => {}
        }
        pos += size;
    }
    Ok(meta)
}

/// `Read::read` until the buffer is full or EOF; returns bytes read.
fn read_up_to(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;

    /// Minimal Skyrim-era TES4 header with the given flags and masters.
    pub(crate) fn fake_plugin(flags: u32, masters: &[&str]) -> Vec<u8> {
        let mut sub = Vec::new();
        // HEDR: version f32, numRecords u32, nextObjectId u32.
        sub.extend_from_slice(b"HEDR");
        sub.extend_from_slice(&12u16.to_le_bytes());
        sub.extend_from_slice(&1.7f32.to_le_bytes());
        sub.extend_from_slice(&0u32.to_le_bytes());
        sub.extend_from_slice(&0u32.to_le_bytes());
        for m in masters {
            sub.extend_from_slice(b"MAST");
            sub.extend_from_slice(&((m.len() + 1) as u16).to_le_bytes());
            sub.extend_from_slice(m.as_bytes());
            sub.push(0);
            // DATA subrecord follows each MAST in real files.
            sub.extend_from_slice(b"DATA");
            sub.extend_from_slice(&8u16.to_le_bytes());
            sub.extend_from_slice(&0u64.to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"TES4");
        out.extend_from_slice(&(sub.len() as u32).to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // formid
        out.extend_from_slice(&0u32.to_le_bytes()); // vc
        out.extend_from_slice(&44u16.to_le_bytes()); // version
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&sub);
        out
    }

    #[test]
    fn parses_masters_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Test.esp");
        fs::write(&p, fake_plugin(0, &["Skyrim.esm", "Update.esm"])).unwrap();
        let meta = plugin_meta(&p).unwrap();
        assert!(!meta.is_master);
        assert_eq!(meta.masters, vec!["Skyrim.esm", "Update.esm"]);

        let p = dir.path().join("Test.esm");
        fs::write(&p, fake_plugin(MASTER_FLAG, &[])).unwrap();
        assert!(plugin_meta(&p).unwrap().is_master);
    }

    #[test]
    fn non_tes4_falls_back_to_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Old.esm");
        fs::write(&p, b"TES3\x00\x00").unwrap();
        let meta = plugin_meta(&p).unwrap();
        assert!(meta.is_master);
        assert!(meta.masters.is_empty());
    }

    #[test]
    fn oblivion_era_header_is_understood() {
        // 20-byte record header: HEDR lands at offset 20.
        let mut out = Vec::new();
        let mut sub = Vec::new();
        sub.extend_from_slice(b"HEDR");
        sub.extend_from_slice(&12u16.to_le_bytes());
        sub.extend_from_slice(&[0u8; 12]);
        sub.extend_from_slice(b"MAST");
        sub.extend_from_slice(&13u16.to_le_bytes());
        sub.extend_from_slice(b"Oblivion.esm\0");
        out.extend_from_slice(b"TES4");
        out.extend_from_slice(&(sub.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&sub);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Mod.esp");
        fs::write(&p, out).unwrap();
        assert_eq!(plugin_meta(&p).unwrap().masters, vec!["Oblivion.esm"]);
    }

    #[test]
    fn sort_is_stable_and_respects_masters() {
        let spec = registry::for_game("skyrimse").unwrap();
        // Simulated analysis: patch loads before its master, esm after esp.
        let mk = |name: &str, is_master: bool, masters: &[&str]| PluginInfo {
            name: name.into(),
            enabled: true,
            present: true,
            official: false,
            meta: PluginMeta {
                is_master,
                is_light: false,
                masters: masters.iter().map(|s| s.to_string()).collect(),
            },
        };
        let analysis = Analysis {
            path: PathBuf::from("/nonexistent/plugins.txt"),
            plugins: vec![
                mk("PatchForB.esp", false, &["B.esm"]),
                mk("A.esp", false, &[]),
                PluginInfo {
                    official: true,
                    ..mk("Skyrim.esm", true, &[])
                },
                mk("B.esm", true, &[]),
            ],
            unlisted: vec![],
            issues: vec![],
            timestamp_caveat: false,
        };
        let plan = plan_sort(&analysis, spec);
        assert!(plan.changed);
        assert_eq!(
            plan.after,
            vec!["Skyrim.esm", "B.esm", "PatchForB.esp", "A.esp"]
        );
    }

    #[test]
    fn read_plugins_both_formats() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plugins.txt");
        fs::write(&p, "# header\n*SkyUI_SE.esp\nDisabled.esp\n").unwrap();
        let ast = registry::for_game("skyrimse").unwrap().plugins.unwrap();
        let got = read_plugins(&p, &ast).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got[0].enabled && got[0].name == "SkyUI_SE.esp");
        assert!(!got[1].enabled);

        let plain = registry::for_game("falloutnv").unwrap().plugins.unwrap();
        fs::write(&p, "FalloutNV.esm\nSomeMod.esp\n").unwrap();
        let got = read_plugins(&p, &plain).unwrap();
        assert!(got.iter().all(|e| e.enabled));
        // Missing file: empty list.
        assert!(
            read_plugins(&dir.path().join("nope.txt"), &plain)
                .unwrap()
                .is_empty()
        );
    }
}
