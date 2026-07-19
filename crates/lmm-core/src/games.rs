//! Built-in registry of games lmm knows how to mod.
//!
//! Adding a game = adding a `GameDef` row here (plus, if its archives have a
//! distinctive layout, a variant in `Layout` handled by `archive::layout`).
//! The registry is mirrored into the `games` table so installations can
//! reference games by stable rowid.

use crate::db::Db;
use crate::error::Result;

/// How mod archives for a game are structured and where files deploy to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Files deploy relative to the game root, archive taken as-is.
    GameRoot,
    /// Bethesda-style: files live under the game's data directory (`Data/`,
    /// `Data Files/` — see `mod_root`; esp/esm/bsa, meshes/, textures/ ...).
    /// Archives may or may not include the data-directory wrapper.
    BethesdaData,
    /// Each top-level directory of the archive is a self-contained mod
    /// folder deployed into `mod_root` as-is (Stardew `Mods/`, Bannerlord
    /// `Modules/`). The top-level folder name is the mod's identity, so
    /// wrapper directories are never unwrapped.
    ModFolder,
}

#[derive(Debug, Clone, Copy)]
pub struct GameDef {
    pub slug: &'static str,
    pub name: &'static str,
    pub steam_app_id: Option<u32>,
    /// Subdirectory of the game root that mod files deploy into ("" = root).
    pub mod_root: &'static str,
    pub layout: Layout,
    /// Game domain on nexusmods.com (the host part of nxm:// links);
    /// None for games without a Nexus section (e.g. `generic`).
    pub nexus_domain: Option<&'static str>,
}

/// Order matters only for display. `slug` and `steam_app_id` must be unique.
pub const REGISTRY: &[GameDef] = &[
    GameDef {
        slug: "skyrimse",
        name: "The Elder Scrolls V: Skyrim Special Edition",
        steam_app_id: Some(489830),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("skyrimspecialedition"),
    },
    GameDef {
        slug: "skyrim",
        name: "The Elder Scrolls V: Skyrim",
        steam_app_id: Some(72850),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("skyrim"),
    },
    GameDef {
        slug: "fallout4",
        name: "Fallout 4",
        steam_app_id: Some(377160),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("fallout4"),
    },
    GameDef {
        slug: "skyrimvr",
        name: "The Elder Scrolls V: Skyrim VR",
        steam_app_id: Some(611670),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        // VR mods are hosted under the flat game's Nexus section; the domain
        // must stay unique here, so nxm links route to the flat game.
        nexus_domain: None,
    },
    GameDef {
        slug: "oblivion",
        name: "The Elder Scrolls IV: Oblivion",
        steam_app_id: Some(22330),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("oblivion"),
    },
    GameDef {
        slug: "morrowind",
        name: "The Elder Scrolls III: Morrowind",
        steam_app_id: Some(22320),
        mod_root: "Data Files",
        layout: Layout::BethesdaData,
        nexus_domain: Some("morrowind"),
    },
    GameDef {
        slug: "fallout3",
        // The GOTY edition (app 22370) is a separate Steam app; register it
        // with 'game add <path> --slug fallout3'.
        name: "Fallout 3",
        steam_app_id: Some(22300),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("fallout3"),
    },
    GameDef {
        slug: "falloutnv",
        name: "Fallout: New Vegas",
        steam_app_id: Some(22380),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("newvegas"),
    },
    GameDef {
        slug: "starfield",
        name: "Starfield",
        steam_app_id: Some(1716740),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("starfield"),
    },
    GameDef {
        slug: "enderalse",
        name: "Enderal: Forgotten Stories (Special Edition)",
        steam_app_id: Some(976620),
        mod_root: "Data",
        layout: Layout::BethesdaData,
        nexus_domain: Some("enderalspecialedition"),
    },
    GameDef {
        slug: "stardewvalley",
        name: "Stardew Valley",
        steam_app_id: Some(413150),
        mod_root: "Mods",
        layout: Layout::ModFolder,
        nexus_domain: Some("stardewvalley"),
    },
    GameDef {
        slug: "cyberpunk2077",
        name: "Cyberpunk 2077",
        steam_app_id: Some(1091500),
        mod_root: "",
        layout: Layout::GameRoot,
        nexus_domain: Some("cyberpunk2077"),
    },
    GameDef {
        slug: "bannerlord",
        name: "Mount & Blade II: Bannerlord",
        steam_app_id: Some(261550),
        mod_root: "Modules",
        layout: Layout::ModFolder,
        nexus_domain: Some("mountandblade2bannerlord"),
    },
    GameDef {
        slug: "7daystodie",
        name: "7 Days to Die",
        steam_app_id: Some(251570),
        mod_root: "Mods",
        layout: Layout::ModFolder,
        nexus_domain: Some("7daystodie"),
    },
    // Catch-all for manually added games lmm has no specific knowledge of.
    GameDef {
        slug: "generic",
        name: "Generic game",
        steam_app_id: None,
        mod_root: "",
        layout: Layout::GameRoot,
        nexus_domain: None,
    },
];

pub fn by_slug(slug: &str) -> Option<&'static GameDef> {
    REGISTRY.iter().find(|g| g.slug == slug)
}

pub fn by_app_id(app_id: u32) -> Option<&'static GameDef> {
    REGISTRY.iter().find(|g| g.steam_app_id == Some(app_id))
}

/// Look a game up by its nexusmods.com domain (the host of an nxm:// link).
pub fn by_nexus_domain(domain: &str) -> Option<&'static GameDef> {
    REGISTRY.iter().find(|g| g.nexus_domain == Some(domain))
}

/// Upsert the registry into the `games` table; returns nothing — callers look
/// games up by slug. Run once at context open.
pub fn sync_registry(db: &Db) -> Result<()> {
    let mut stmt = db.conn.prepare(
        "INSERT INTO games (slug, name, steam_app_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(slug) DO UPDATE SET name = excluded.name,
                                         steam_app_id = excluded.steam_app_id",
    )?;
    for def in REGISTRY {
        stmt.execute(rusqlite::params![def.slug, def.name, def.steam_app_id])?;
    }
    Ok(())
}

/// Database id of a game by slug (registry is synced at open, so this
/// succeeds for any registry slug).
pub fn game_id(db: &Db, slug: &str) -> Result<i64> {
    Ok(db
        .conn
        .query_row("SELECT id FROM games WHERE slug = ?1", [slug], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_consistent() {
        let mut slugs: Vec<_> = REGISTRY.iter().map(|g| g.slug).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), REGISTRY.len(), "duplicate slug");

        let mut apps: Vec<_> = REGISTRY.iter().filter_map(|g| g.steam_app_id).collect();
        apps.sort_unstable();
        apps.dedup();
        let n_apps = REGISTRY.iter().filter(|g| g.steam_app_id.is_some()).count();
        assert_eq!(apps.len(), n_apps, "duplicate steam app id");

        // nxm links resolve games by domain, so duplicates would be ambiguous.
        let mut domains: Vec<_> = REGISTRY.iter().filter_map(|g| g.nexus_domain).collect();
        domains.sort_unstable();
        domains.dedup();
        let n_domains = REGISTRY.iter().filter(|g| g.nexus_domain.is_some()).count();
        assert_eq!(domains.len(), n_domains, "duplicate nexus domain");
    }

    #[test]
    fn sync_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        sync_registry(&db).unwrap();
        sync_registry(&db).unwrap();
        assert!(game_id(&db, "skyrimse").unwrap() > 0);
        assert!(by_app_id(489830).is_some());
        assert!(by_slug("nope").is_none());
    }
}
