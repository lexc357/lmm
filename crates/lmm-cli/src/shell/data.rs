//! Candidate data for shell completion: a cached snapshot of the
//! application state, refreshed once per prompt.
//!
//! Why a snapshot instead of querying per keystroke: completion must feel
//! immediate, and application state can only change *between* readline
//! calls — commands execute between prompts, and the only asynchronous
//! writers (download workers) merely tick ids/statuses that a one-prompt
//! lag cannot make wrong. Refreshing right before each `read_line` (a
//! handful of small indexed queries through the same lmm-core services the
//! commands use — no SQL lives here) therefore gives always-current data
//! with zero per-keystroke cost and no invalidation bookkeeping: install,
//! remove, enable, disable, profile switches and game changes are all
//! visible at the very next prompt.
//!
//! Failures never break the shell: a failed refresh keeps the previous
//! snapshot (or an empty one), records the error for diagnostics, and the
//! engine falls back to static completions.

use lmm_core::{Context, Overrides, installs, mods, profile};
use lmm_nexus::queue;

/// One installed mod with the state completion filters on.
#[derive(Debug, Clone)]
pub struct ModEntry {
    pub name: String,
    pub enabled: bool,
}

/// One download row, pre-formatted for suggestion display.
#[derive(Debug, Clone)]
pub struct DownloadEntry {
    pub id: i64,
    pub status: queue::Status,
    /// Short human label ("'SkyUI' (SkyUI_5_2.7z)" or ids).
    pub label: String,
}

/// One Game Tools catalog entry for the current game.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub id: String,
    pub name: String,
}

/// Everything the completion engine may suggest, as plain data.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Selectors for every registered installation: labels, slugs, ids.
    pub installs: Vec<String>,
    /// Mods of the current default installation, active-profile state.
    pub mods: Vec<ModEntry>,
    /// Profiles of the current default installation.
    pub profiles: Vec<String>,
    pub downloads: Vec<DownloadEntry>,
    /// Tool catalog of the current default installation's game.
    pub tools: Vec<ToolEntry>,
    /// Why (part of) the snapshot could not be refreshed, if anything.
    pub last_error: Option<String>,
}

/// Owns a dedicated database connection (via its own [`Context`]) so the
/// completion machinery never contends with the command being executed.
pub struct CompletionData {
    ctx: Context,
    snapshot: Snapshot,
}

impl CompletionData {
    pub fn new(overrides: &Overrides) -> lmm_core::error::Result<CompletionData> {
        Ok(CompletionData {
            ctx: Context::open(overrides)?,
            snapshot: Snapshot::default(),
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Rebuild the snapshot. Partial failure keeps whatever loaded and
    /// records the first error; the caller may surface it in verbose mode.
    pub fn refresh(&mut self) {
        let mut snap = Snapshot::default();

        match installs::list(&self.ctx.db) {
            Ok(list) => {
                for inst in &list {
                    snap.installs.extend(inst.label.clone());
                    snap.installs.push(inst.game_slug.clone());
                    snap.installs.push(inst.id.to_string());
                }
                snap.installs.dedup();
            }
            Err(e) => record(&mut snap, e),
        }

        // Mods and profiles are relative to the current default installation;
        // no default (or none registered) simply means nothing to suggest.
        if let Ok(inst) = installs::select(&self.ctx.db, None) {
            match mods::active_profile_id(&self.ctx, &inst)
                .and_then(|pid| mods::list_for_profile(&self.ctx.db, pid))
            {
                Ok(list) => {
                    snap.mods = list
                        .into_iter()
                        .map(|m| ModEntry {
                            name: m.info.name,
                            enabled: m.enabled,
                        })
                        .collect();
                }
                Err(e) => record(&mut snap, e),
            }
            match profile::list(&self.ctx.db, inst.id) {
                Ok(list) => snap.profiles = list.into_iter().map(|p| p.name).collect(),
                Err(e) => record(&mut snap, e),
            }
            // Static catalog data; a game without one simply suggests nothing.
            if let Some(game) = lmm_core::tools::registry::for_game(&inst.game_slug) {
                snap.tools = game
                    .tools
                    .iter()
                    .map(|t| ToolEntry {
                        id: t.id.to_string(),
                        name: t.name.to_string(),
                    })
                    .collect();
            }
        }

        match queue::list(&self.ctx.db, None) {
            Ok(list) => {
                snap.downloads = list
                    .into_iter()
                    .map(|d| DownloadEntry {
                        id: d.id,
                        status: d.status,
                        label: d.describe(),
                    })
                    .collect();
            }
            Err(e) => record(&mut snap, e),
        }

        self.snapshot = snap;
    }
}

/// Keep the first refresh error for diagnostics; later ones add no signal.
fn record(snap: &mut Snapshot, e: impl std::fmt::Display) {
    if snap.last_error.is_none() {
        snap.last_error = Some(e.to_string());
    }
}
